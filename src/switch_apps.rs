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
    first_window_is_iconic: bool,
) -> Option<usize> {
    if windows.is_empty() {
        return None;
    }
    if first_window_is_iconic && windows.len() > 1 {
        return Some(windows.len() - 1);
    }
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
        assert_eq!(representative_window_index(&windows, false), Some(0));
    }

    #[test]
    fn representative_window_index_falls_back_to_last_window_when_first_is_iconic() {
        let windows = [1, 2, 3];
        assert_eq!(representative_window_index(&windows, true), Some(2));
    }

    #[test]
    fn representative_window_index_handles_empty_groups() {
        let windows: [usize; 0] = [];
        assert_eq!(representative_window_index(&windows, false), None);
    }

    #[test]
    fn app_switch_entry_uses_same_window_for_preview_and_activation() {
        let representative_hwnd = fake_hwnd(42);
        let entry = AppSwitchEntry::new(
            "C:\\Program Files\\App\\app.exe".into(),
            fake_hicon(7),
            representative_hwnd,
            vec![
                AppSwitchWindow::new(fake_hwnd(41), "Older".into()),
                AppSwitchWindow::new(representative_hwnd, "Current".into()),
            ],
        );

        assert_eq!(entry.preview_hwnd(), representative_hwnd);
        assert_eq!(entry.windows.len(), 2);
        assert_eq!(entry.module_path, "C:\\Program Files\\App\\app.exe");
        assert_eq!(
            entry
                .windows
                .iter()
                .find(|window| window.hwnd == representative_hwnd)
                .map(|window| window.title.as_str()),
            Some("Current")
        );
    }
}
