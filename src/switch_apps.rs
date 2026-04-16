use crate::config::SwitchAppsRenderMode;
use crate::preview::AppPreview;

use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::HICON};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RepresentativeWindowPolicy {
    #[default]
    LegacyMinimizedFallback,
    FirstWindow,
}

impl RepresentativeWindowPolicy {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "legacy" | "legacy_minimized_fallback" => Some(Self::LegacyMinimizedFallback),
            "first" | "first_window" => Some(Self::FirstWindow),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSwitchWindow {
    pub hwnd: HWND,
    pub title: String,
}

impl AppSwitchWindow {
    pub fn new(hwnd: HWND, title: String) -> Self {
        Self { hwnd, title }
    }
}

#[derive(Debug, Clone)]
pub struct AppSwitchEntry {
    #[allow(dead_code)]
    pub module_path: String,
    pub icon: HICON,
    pub representative_hwnd: HWND,
    #[allow(dead_code)]
    pub preview: AppPreview,
    #[allow(dead_code)]
    pub windows: Vec<AppSwitchWindow>,
}

impl AppSwitchEntry {
    pub fn new(
        module_path: String,
        icon: HICON,
        representative_hwnd: HWND,
        preview: AppPreview,
        windows: Vec<AppSwitchWindow>,
    ) -> Self {
        debug_assert!(
            windows
                .iter()
                .any(|window| window.hwnd == representative_hwnd),
            "representative window must be part of the app entry"
        );
        debug_assert!(
            matches!(preview, AppPreview::Unavailable(_))
                || matches!(
                    preview,
                    AppPreview::DwmThumbnail(preview)
                        if preview.source_hwnd == representative_hwnd
                ),
            "preview source must match the representative window"
        );
        Self {
            module_path,
            icon,
            representative_hwnd,
            preview,
            windows,
        }
    }

    #[cfg(test)]
    pub fn from_windows(
        module_path: String,
        icon: HICON,
        preview: AppPreview,
        windows: Vec<AppSwitchWindow>,
        policy: RepresentativeWindowPolicy,
        is_iconic: impl FnMut(&AppSwitchWindow) -> bool,
    ) -> Option<Self> {
        let representative_index = representative_window_index(&windows, policy, is_iconic)?;
        let representative_hwnd = windows[representative_index].hwnd;
        Some(Self::new(
            module_path,
            icon,
            representative_hwnd,
            preview,
            windows,
        ))
    }

    #[allow(dead_code)]
    pub fn preview_hwnd(&self) -> HWND {
        self.representative_hwnd
    }

    pub fn hwnd_for_window_index(&self, window_index: usize) -> HWND {
        self.windows
            .get(window_index)
            .map(|window| window.hwnd)
            .unwrap_or(self.representative_hwnd)
    }
}

#[derive(Debug, Clone)]
pub struct SwitchAppsState {
    pub apps: Vec<AppSwitchEntry>,
    pub index: usize,
    /// Index of the selected window within the currently selected app.
    /// `0` means the representative window. Updated by inline `Alt+\`` cycling.
    pub window_index: usize,
    pub render_mode: SwitchAppsRenderMode,
    pub show_window_count: bool,
    pub overlay_scale: u32,
    pub backdrop_opacity: u32,
    pub backdrop_color: Option<u32>,
}

impl SwitchAppsState {
    pub fn selected_app(&self) -> Option<&AppSwitchEntry> {
        self.apps.get(self.index)
    }

    pub fn selected_hwnd(&self) -> Option<HWND> {
        self.selected_app()
            .map(|app| app.hwnd_for_window_index(self.window_index))
    }

    pub fn preview_hwnd_for_app(&self, app_index: usize) -> Option<HWND> {
        let app = self.apps.get(app_index)?;
        if app_index == self.index {
            Some(app.hwnd_for_window_index(self.window_index))
        } else {
            Some(app.representative_hwnd)
        }
    }

    pub fn cycle_window(&mut self, reverse: bool) {
        let Some(app) = self.apps.get(self.index) else {
            return;
        };
        let count = app.windows.len();
        if count <= 1 {
            return;
        }
        if reverse {
            self.window_index = if self.window_index == 0 {
                count - 1
            } else {
                self.window_index - 1
            };
        } else {
            self.window_index = if self.window_index >= count - 1 {
                0
            } else {
                self.window_index + 1
            };
        }
    }

    pub fn reset_window_index(&mut self) {
        self.window_index = 0;
    }
}

pub fn representative_window_index<T>(
    windows: &[T],
    policy: RepresentativeWindowPolicy,
    mut is_iconic: impl FnMut(&T) -> bool,
) -> Option<usize> {
    if windows.is_empty() {
        return None;
    }
    match policy {
        RepresentativeWindowPolicy::LegacyMinimizedFallback => {
            if windows.len() > 1 && is_iconic(&windows[0]) {
                Some(windows.len() - 1)
            } else {
                Some(0)
            }
        }
        RepresentativeWindowPolicy::FirstWindow => {
            // `list_windows()` preserves the shell order, so the first window in the
            // group is the app's representative window for both preview and activation.
            Some(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        representative_window_index, AppSwitchEntry, AppSwitchWindow, RepresentativeWindowPolicy,
    };
    use crate::preview::{AppPreview, DwmThumbnailPreview, PreviewUnavailableReason};
    use core::ffi::c_void;
    use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::HICON};

    fn fake_hwnd(value: usize) -> HWND {
        HWND(value as *mut c_void)
    }

    fn fake_hicon(value: usize) -> HICON {
        HICON(value as *mut c_void)
    }

    #[test]
    fn representative_window_index_legacy_policy_prefers_first_when_not_iconic() {
        let windows = [1, 2, 3];
        assert_eq!(
            representative_window_index(
                &windows,
                RepresentativeWindowPolicy::LegacyMinimizedFallback,
                |_| false
            ),
            Some(0)
        );
    }

    #[test]
    fn representative_window_index_legacy_policy_falls_back_to_last_when_first_is_iconic() {
        let windows = [1, 2, 3];
        assert_eq!(
            representative_window_index(
                &windows,
                RepresentativeWindowPolicy::LegacyMinimizedFallback,
                |value| *value == 1
            ),
            Some(2)
        );
    }

    #[test]
    fn representative_window_index_first_window_policy_always_uses_first_window() {
        let windows = [3, 2, 1];
        assert_eq!(
            representative_window_index(&windows, RepresentativeWindowPolicy::FirstWindow, |_| {
                true
            }),
            Some(0)
        );
    }

    #[test]
    fn representative_window_index_handles_empty_groups() {
        let windows: [usize; 0] = [];
        assert_eq!(
            representative_window_index(
                &windows,
                RepresentativeWindowPolicy::LegacyMinimizedFallback,
                |_| false
            ),
            None
        );
        assert_eq!(
            representative_window_index(&windows, RepresentativeWindowPolicy::FirstWindow, |_| {
                false
            }),
            None
        );
    }

    #[test]
    fn app_switch_entry_from_windows_rejects_empty_groups() {
        let entry = AppSwitchEntry::from_windows(
            "C:\\Program Files\\App\\app.exe".into(),
            fake_hicon(7),
            AppPreview::Unavailable(PreviewUnavailableReason::DisabledByConfig),
            vec![],
            RepresentativeWindowPolicy::LegacyMinimizedFallback,
            |_| false,
        );

        assert!(entry.is_none());
    }

    #[test]
    fn app_switch_entry_first_window_policy_uses_same_window_for_preview_and_activation() {
        let first_hwnd = fake_hwnd(42);
        let entry = AppSwitchEntry::from_windows(
            "C:\\Program Files\\App\\app.exe".into(),
            fake_hicon(7),
            AppPreview::DwmThumbnail(DwmThumbnailPreview::new(first_hwnd)),
            vec![
                AppSwitchWindow::new(first_hwnd, "Current".into()),
                AppSwitchWindow::new(fake_hwnd(41), "Older".into()),
            ],
            RepresentativeWindowPolicy::FirstWindow,
            |_| false,
        );
        let entry = entry.expect("entry should be created");

        assert_eq!(entry.preview_hwnd(), first_hwnd);
        assert_eq!(
            entry.preview,
            AppPreview::DwmThumbnail(DwmThumbnailPreview::new(first_hwnd))
        );
        assert_eq!(entry.windows.len(), 2);
        assert_eq!(entry.module_path, "C:\\Program Files\\App\\app.exe");
        assert_eq!(
            entry
                .windows
                .iter()
                .find(|window| window.hwnd == first_hwnd)
                .map(|window| window.title.as_str()),
            Some("Current")
        );
    }

    #[test]
    fn app_switch_entry_legacy_policy_preserves_minimized_fallback() {
        let fallback_hwnd = fake_hwnd(41);
        let entry = AppSwitchEntry::from_windows(
            "C:\\Program Files\\App\\app.exe".into(),
            fake_hicon(7),
            AppPreview::Unavailable(PreviewUnavailableReason::Minimized),
            vec![
                AppSwitchWindow::new(fake_hwnd(42), "Minimized".into()),
                AppSwitchWindow::new(fallback_hwnd, "Fallback".into()),
            ],
            RepresentativeWindowPolicy::LegacyMinimizedFallback,
            |window| window.title == "Minimized",
        )
        .expect("entry should be created");

        assert_eq!(entry.preview_hwnd(), fallback_hwnd);
        assert_eq!(
            entry.preview,
            AppPreview::Unavailable(PreviewUnavailableReason::Minimized)
        );
    }

    fn make_test_state(window_counts: &[usize]) -> super::SwitchAppsState {
        use crate::config::SwitchAppsRenderMode;

        let apps = window_counts
            .iter()
            .enumerate()
            .map(|(app_idx, &count)| {
                let windows: Vec<AppSwitchWindow> = (0..count)
                    .map(|win_idx| {
                        AppSwitchWindow::new(
                            fake_hwnd(app_idx * 100 + win_idx + 1),
                            format!("Window {win_idx}"),
                        )
                    })
                    .collect();
                let representative_hwnd = windows[0].hwnd;
                AppSwitchEntry::new(
                    format!("app{app_idx}.exe"),
                    fake_hicon(app_idx + 1),
                    representative_hwnd,
                    AppPreview::Unavailable(PreviewUnavailableReason::DisabledByConfig),
                    windows,
                )
            })
            .collect();

        super::SwitchAppsState {
            apps,
            index: 0,
            window_index: 0,
            render_mode: SwitchAppsRenderMode::IconOnly,
            show_window_count: false,
            overlay_scale: 100,
            backdrop_opacity: 100,
            backdrop_color: None,
        }
    }

    #[test]
    fn cycle_window_advances_within_selected_app() {
        let mut state = make_test_state(&[3, 1]);

        assert_eq!(state.window_index, 0);
        state.cycle_window(false);
        assert_eq!(state.window_index, 1);
        state.cycle_window(false);
        assert_eq!(state.window_index, 2);
        state.cycle_window(false);
        assert_eq!(state.window_index, 0);
    }

    #[test]
    fn cycle_window_reverses_within_selected_app() {
        let mut state = make_test_state(&[3, 1]);

        assert_eq!(state.window_index, 0);
        state.cycle_window(true);
        assert_eq!(state.window_index, 2);
        state.cycle_window(true);
        assert_eq!(state.window_index, 1);
    }

    #[test]
    fn cycle_window_is_noop_for_single_window_app() {
        let mut state = make_test_state(&[1, 3]);

        state.cycle_window(false);
        assert_eq!(state.window_index, 0);
        state.cycle_window(true);
        assert_eq!(state.window_index, 0);
    }

    #[test]
    fn selected_hwnd_follows_window_index() {
        let mut state = make_test_state(&[3, 1]);
        let expected_first = state.apps[0].windows[0].hwnd;
        let expected_second = state.apps[0].windows[1].hwnd;

        assert_eq!(state.selected_hwnd(), Some(expected_first));
        state.cycle_window(false);
        assert_eq!(state.selected_hwnd(), Some(expected_second));
    }

    #[test]
    fn reset_window_index_returns_to_representative() {
        let mut state = make_test_state(&[3, 1]);

        state.cycle_window(false);
        state.cycle_window(false);
        assert_eq!(state.window_index, 2);

        state.reset_window_index();
        assert_eq!(state.window_index, 0);
        assert_eq!(state.selected_hwnd(), Some(state.apps[0].windows[0].hwnd));
    }

    #[test]
    fn switching_apps_resets_window_index_conceptually() {
        let mut state = make_test_state(&[3, 2]);

        state.cycle_window(false);
        assert_eq!(state.window_index, 1);

        // Simulate switching to next app.
        state.index = 1;
        state.reset_window_index();
        assert_eq!(state.window_index, 0);
        assert_eq!(state.selected_hwnd(), Some(state.apps[1].windows[0].hwnd));
    }

    #[test]
    fn selected_hwnd_falls_back_to_representative_for_out_of_range_window_index() {
        let mut state = make_test_state(&[2, 1]);
        // Manually set window_index out of range to simulate a stale index.
        state.window_index = 99;
        assert_eq!(
            state.selected_hwnd(),
            Some(state.apps[0].representative_hwnd)
        );
    }

    #[test]
    fn preview_hwnd_for_selected_app_follows_window_index() {
        let mut state = make_test_state(&[3, 2]);
        let selected_preview = state.apps[0].windows[0].hwnd;
        let cycled_preview = state.apps[0].windows[2].hwnd;

        assert_eq!(state.preview_hwnd_for_app(0), Some(selected_preview));

        state.cycle_window(false);
        state.cycle_window(false);

        assert_eq!(state.preview_hwnd_for_app(0), Some(cycled_preview));
    }

    #[test]
    fn preview_hwnd_for_other_apps_stays_on_representative_window() {
        let mut state = make_test_state(&[2, 3]);
        let other_representative = state.apps[1].representative_hwnd;

        state.cycle_window(false);

        assert_eq!(state.preview_hwnd_for_app(1), Some(other_representative));
    }

    #[test]
    fn preview_hwnd_for_selected_app_falls_back_to_representative_when_stale() {
        let mut state = make_test_state(&[2, 1]);
        state.window_index = 99;

        assert_eq!(
            state.preview_hwnd_for_app(0),
            Some(state.apps[0].representative_hwnd)
        );
    }

    #[test]
    fn cycle_window_wraps_forward_and_backward_with_two_windows() {
        let mut state = make_test_state(&[2]);
        let w0 = state.apps[0].windows[0].hwnd;
        let w1 = state.apps[0].windows[1].hwnd;

        assert_eq!(state.selected_hwnd(), Some(w0));
        state.cycle_window(false);
        assert_eq!(state.selected_hwnd(), Some(w1));
        state.cycle_window(false);
        assert_eq!(state.selected_hwnd(), Some(w0));

        state.cycle_window(true);
        assert_eq!(state.selected_hwnd(), Some(w1));
        state.cycle_window(true);
        assert_eq!(state.selected_hwnd(), Some(w0));
    }

    #[test]
    fn selected_app_returns_none_for_empty_state() {
        let state = make_test_state(&[]);
        assert!(state.selected_app().is_none());
        assert!(state.selected_hwnd().is_none());
    }
}
