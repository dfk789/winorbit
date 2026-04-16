use anyhow::{Context, Result};
use window_switcher::utils::*;

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::Graphics::Dwm::{DWM_CLOAKED_APP, DWM_CLOAKED_INHERITED, DWM_CLOAKED_SHELL};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindow, GW_OWNER};

fn main() -> Result<()> {
    let is_admin = is_running_as_admin().unwrap_or(false);
    let mut hwnds: Vec<HWND> = Default::default();
    unsafe { EnumWindows(Some(enum_window), LPARAM(&mut hwnds as *mut _ as isize)) }
        .with_context(|| "Fail to enum windows".to_string())?;
    for hwnd in hwnds {
        let title = get_window_title(hwnd);
        let cloak_type = get_window_cloak_type(hwnd);
        let (is_visible, is_iconic, is_tool, is_topmost) = get_window_state(hwnd);
        let (width, height) = get_window_size(hwnd);
        let owner_hwnd: HWND = unsafe { GetWindow(hwnd, GW_OWNER) }.unwrap_or_default();
        let owner_title = if !owner_hwnd.is_invalid() {
            get_window_title(owner_hwnd)
        } else {
            "".into()
        };
        let pid = get_window_pid(hwnd);
        let module_path = get_module_path(pid).unwrap_or_default();
        let elevated = is_process_elevated(pid);
        let filter_reason = window_filter_reason(
            WindowFilterInput {
                owner_hwnd,
                is_visible,
                is_iconic,
                is_tool,
                is_topmost,
                is_cloaked: window_is_cloaked_for_switching(cloak_type, false),
                is_small: is_small_window(hwnd),
            },
            &title,
            false,
        );
        let status = if let Some(reason) = filter_reason {
            format!("filter:{}", describe_filter_reason(reason))
        } else if !is_valid_module_path(&module_path) {
            "filter:invalid-module".to_string()
        } else if !is_admin && elevated == Some(true) {
            "filter:elevated".to_string()
        } else {
            "filter:include".to_string()
        };
        println!(
            "{:<22} elev:{} visible:{} iconic:{} tool:{} top:{} cloak:{} {:>10} {:>10} pid:{:<6} owner:{:<10} module:{} title:{} owner_title:{}",
            status,
            pretty_optional_bool(elevated),
            pretty_bool(is_visible),
            pretty_bool(is_iconic),
            pretty_bool(is_tool),
            pretty_bool(is_topmost),
            pretty_cloak(cloak_type),
            format!("{}x{}", width, height),
            hwnd.0 as isize,
            pid,
            owner_hwnd.0 as isize,
            if module_path.is_empty() {
                "<none>"
            } else {
                module_path.as_str()
            },
            title,
            owner_title,
        );
    }
    Ok(())
}

fn pretty_bool(value: bool) -> String {
    if value {
        "*".into()
    } else {
        " ".into()
    }
}

fn pretty_optional_bool(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "*",
        Some(false) => " ",
        None => "?",
    }
}

fn pretty_cloak(value: u32) -> &'static str {
    match value {
        0 => " ",
        DWM_CLOAKED_SHELL => "S",
        DWM_CLOAKED_APP => "A",
        DWM_CLOAKED_INHERITED => "I",
        _ => "?",
    }
}

fn describe_filter_reason(reason: WindowFilterReason) -> &'static str {
    match reason {
        WindowFilterReason::NotVisible => "not-visible",
        WindowFilterReason::Minimized => "minimized",
        WindowFilterReason::ToolWindow => "tool-window",
        WindowFilterReason::OwnedTopmostWindow => "owned-topmost",
        WindowFilterReason::Cloaked => "cloaked",
        WindowFilterReason::Small => "small",
        WindowFilterReason::EmptyTitle => "empty-title",
        WindowFilterReason::WindowsInputExperience => "windows-input-experience",
    }
}

extern "system" fn enum_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let windows: &mut Vec<HWND> = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    windows.push(hwnd);
    BOOL(1)
}
