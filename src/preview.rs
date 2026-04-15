use crate::config::SwitchAppsRenderMode;
use crate::utils::{get_window_cloak_type, is_iconic_window};

use windows::Win32::Foundation::HWND;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppPreview {
    DwmThumbnail(DwmThumbnailPreview),
    Unavailable(PreviewUnavailableReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DwmThumbnailPreview {
    pub source_hwnd: HWND,
}

impl DwmThumbnailPreview {
    pub fn new(source_hwnd: HWND) -> Self {
        Self { source_hwnd }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewUnavailableReason {
    DisabledByConfig,
    Minimized,
    Cloaked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewWindowState {
    pub is_iconic: bool,
    pub cloak_type: u32,
}

impl PreviewWindowState {
    pub fn probe(hwnd: HWND) -> Self {
        Self {
            is_iconic: is_iconic_window(hwnd),
            cloak_type: get_window_cloak_type(hwnd),
        }
    }

    pub fn is_cloaked(&self) -> bool {
        self.cloak_type != 0
    }
}

pub trait PreviewSource {
    fn preview_for_hwnd(&self, hwnd: HWND) -> AppPreview;
}

#[derive(Debug, Clone, Copy)]
pub struct WindowPreviewSource {
    render_mode: SwitchAppsRenderMode,
}

impl WindowPreviewSource {
    pub fn new(render_mode: SwitchAppsRenderMode) -> Self {
        Self { render_mode }
    }
}

impl PreviewSource for WindowPreviewSource {
    fn preview_for_hwnd(&self, hwnd: HWND) -> AppPreview {
        if self.render_mode != SwitchAppsRenderMode::Preview {
            return AppPreview::Unavailable(PreviewUnavailableReason::DisabledByConfig);
        }
        resolve_preview(self.render_mode, hwnd, PreviewWindowState::probe(hwnd))
    }
}

pub fn resolve_preview(
    render_mode: SwitchAppsRenderMode,
    hwnd: HWND,
    state: PreviewWindowState,
) -> AppPreview {
    match render_mode {
        SwitchAppsRenderMode::IconOnly => {
            AppPreview::Unavailable(PreviewUnavailableReason::DisabledByConfig)
        }
        SwitchAppsRenderMode::Preview => {
            if state.is_iconic {
                AppPreview::Unavailable(PreviewUnavailableReason::Minimized)
            } else if state.is_cloaked() {
                AppPreview::Unavailable(PreviewUnavailableReason::Cloaked)
            } else {
                // This is a descriptor only. S03D owns any future DWM thumbnail
                // registration handles and their cleanup when the painter renders it.
                AppPreview::DwmThumbnail(DwmThumbnailPreview::new(hwnd))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        resolve_preview, AppPreview, DwmThumbnailPreview, PreviewUnavailableReason,
        PreviewWindowState, WindowPreviewSource,
    };
    use crate::config::SwitchAppsRenderMode;
    use crate::preview::PreviewSource;
    use core::ffi::c_void;
    use windows::Win32::Foundation::HWND;

    fn fake_hwnd(value: usize) -> HWND {
        HWND(value as *mut c_void)
    }

    #[test]
    fn resolve_preview_disables_preview_in_icon_only_mode() {
        let hwnd = fake_hwnd(1);
        let preview = resolve_preview(
            SwitchAppsRenderMode::IconOnly,
            hwnd,
            PreviewWindowState {
                is_iconic: false,
                cloak_type: 0,
            },
        );

        assert_eq!(
            preview,
            AppPreview::Unavailable(PreviewUnavailableReason::DisabledByConfig)
        );
    }

    #[test]
    fn resolve_preview_marks_minimized_windows_unavailable() {
        let hwnd = fake_hwnd(1);
        let preview = resolve_preview(
            SwitchAppsRenderMode::Preview,
            hwnd,
            PreviewWindowState {
                is_iconic: true,
                cloak_type: 0,
            },
        );

        assert_eq!(
            preview,
            AppPreview::Unavailable(PreviewUnavailableReason::Minimized)
        );
    }

    #[test]
    fn resolve_preview_marks_cloaked_windows_unavailable() {
        let hwnd = fake_hwnd(1);
        let preview = resolve_preview(
            SwitchAppsRenderMode::Preview,
            hwnd,
            PreviewWindowState {
                is_iconic: false,
                cloak_type: 1,
            },
        );

        assert_eq!(
            preview,
            AppPreview::Unavailable(PreviewUnavailableReason::Cloaked)
        );
    }

    #[test]
    fn resolve_preview_uses_dwm_thumbnail_for_eligible_windows() {
        let hwnd = fake_hwnd(7);
        let preview = resolve_preview(
            SwitchAppsRenderMode::Preview,
            hwnd,
            PreviewWindowState {
                is_iconic: false,
                cloak_type: 0,
            },
        );

        assert_eq!(
            preview,
            AppPreview::DwmThumbnail(DwmThumbnailPreview::new(hwnd))
        );
    }

    #[test]
    fn window_preview_source_short_circuits_when_preview_mode_is_disabled() {
        let source = WindowPreviewSource::new(SwitchAppsRenderMode::IconOnly);

        assert_eq!(
            source.preview_for_hwnd(fake_hwnd(9)),
            AppPreview::Unavailable(PreviewUnavailableReason::DisabledByConfig)
        );
    }
}
