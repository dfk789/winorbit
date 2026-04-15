use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::HICON};

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
    ) -> Option<Self> {
        let representative_index = representative_window_index(&windows)?;
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

pub fn representative_window_index<T>(windows: &[T]) -> Option<usize> {
    if windows.is_empty() {
        return None;
    }
    // `list_windows()` preserves the shell order, so the first window in the group
    // is the app's representative window for both preview and activation.
    Some(0)
}

#[cfg(test)]
mod tests {
    use super::{representative_window_index, AppSwitchEntry, AppSwitchWindow};
    use core::ffi::c_void;
    use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::HICON};

    fn fake_hwnd(value: usize) -> HWND {
        HWND(value as *mut c_void)
    }

    fn fake_hicon(value: usize) -> HICON {
        HICON(value as *mut c_void)
    }

    #[test]
    fn representative_window_index_prefers_first_window_when_not_iconic() {
        let windows = [1, 2, 3];
        assert_eq!(representative_window_index(&windows), Some(0));
    }

    #[test]
    fn representative_window_index_always_uses_first_window_in_group_order() {
        let windows = [3, 2, 1];
        assert_eq!(representative_window_index(&windows), Some(0));
    }

    #[test]
    fn representative_window_index_handles_empty_groups() {
        let windows: [usize; 0] = [];
        assert_eq!(representative_window_index(&windows), None);
    }

    #[test]
    fn app_switch_entry_from_windows_rejects_empty_groups() {
        let entry = AppSwitchEntry::from_windows(
            "C:\\Program Files\\App\\app.exe".into(),
            fake_hicon(7),
            vec![],
        );

        assert!(entry.is_none());
    }

    #[test]
    fn app_switch_entry_uses_same_window_for_preview_and_activation() {
        let most_recent_hwnd = fake_hwnd(42);
        let entry = AppSwitchEntry::from_windows(
            "C:\\Program Files\\App\\app.exe".into(),
            fake_hicon(7),
            vec![
                AppSwitchWindow::new(most_recent_hwnd, "Current".into()),
                AppSwitchWindow::new(fake_hwnd(41), "Older".into()),
            ],
        );
        let entry = entry.expect("entry should be created");

        assert_eq!(entry.preview_hwnd(), most_recent_hwnd);
        assert_eq!(entry.windows.len(), 2);
        assert_eq!(entry.module_path, "C:\\Program Files\\App\\app.exe");
        assert_eq!(
            entry
                .windows
                .iter()
                .find(|window| window.hwnd == most_recent_hwnd)
                .map(|window| window.title.as_str()),
            Some("Current")
        );
    }
}
