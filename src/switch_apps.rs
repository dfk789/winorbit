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
    pub windows: Vec<AppSwitchWindow>,
}

impl AppSwitchEntry {
    pub fn new(
        module_path: String,
        icon: HICON,
        representative_hwnd: HWND,
        windows: Vec<AppSwitchWindow>,
    ) -> Self {
        debug_assert!(
            windows
                .iter()
                .any(|window| window.hwnd == representative_hwnd),
            "representative window must be part of the app entry"
        );
        Self {
            module_path,
            icon,
            representative_hwnd,
            windows,
        }
    }

    pub fn from_windows(
        module_path: String,
        icon: HICON,
        windows: Vec<AppSwitchWindow>,
        policy: RepresentativeWindowPolicy,
        is_iconic: impl FnMut(&AppSwitchWindow) -> bool,
    ) -> Option<Self> {
        let representative_index = representative_window_index(&windows, policy, is_iconic)?;
        let representative_hwnd = windows[representative_index].hwnd;
        Some(Self::new(module_path, icon, representative_hwnd, windows))
    }

    pub fn preview_hwnd(&self) -> HWND {
        self.representative_hwnd
    }
}

#[derive(Debug, Clone)]
pub struct SwitchAppsState {
    pub apps: Vec<AppSwitchEntry>,
    pub index: usize,
}

impl SwitchAppsState {
    pub fn selected_app(&self) -> Option<&AppSwitchEntry> {
        self.apps.get(self.index)
    }

    pub fn selected_hwnd(&self) -> Option<HWND> {
        self.selected_app().map(AppSwitchEntry::preview_hwnd)
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
            vec![
                AppSwitchWindow::new(first_hwnd, "Current".into()),
                AppSwitchWindow::new(fake_hwnd(41), "Older".into()),
            ],
            RepresentativeWindowPolicy::FirstWindow,
            |_| false,
        );
        let entry = entry.expect("entry should be created");

        assert_eq!(entry.preview_hwnd(), first_hwnd);
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
            vec![
                AppSwitchWindow::new(fake_hwnd(42), "Minimized".into()),
                AppSwitchWindow::new(fallback_hwnd, "Fallback".into()),
            ],
            RepresentativeWindowPolicy::LegacyMinimizedFallback,
            |window| window.title == "Minimized",
        )
        .expect("entry should be created");

        assert_eq!(entry.preview_hwnd(), fallback_hwnd);
    }
}
