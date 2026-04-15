use crate::config::SwitchAppsRenderMode;
use crate::switch_apps::SwitchAppsState;
use crate::utils::{check_error, get_moinitor_rect, is_light_theme, is_win11};

use anyhow::{Context, Result};
use windows::Win32::{
    Foundation::{COLORREF, HWND, POINT, RECT, SIZE},
    Graphics::{
        Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, CreateRoundRectRgn, CreateSolidBrush,
            DeleteDC, DeleteObject, FillRect, FillRgn, GetDC, ReleaseDC, SelectObject,
            SetStretchBltMode, StretchBlt, AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION, HALFTONE,
            HBITMAP, HDC, HPALETTE, SRCCOPY,
        },
        GdiPlus::{
            FillModeAlternate, GdipAddPathArc, GdipClosePathFigure, GdipCreateBitmapFromHBITMAP,
            GdipCreateFromHDC, GdipCreatePath, GdipCreatePen1, GdipDeleteBrush, GdipDeleteGraphics,
            GdipDeletePath, GdipDeletePen, GdipDisposeImage, GdipDrawImageRect, GdipFillPath,
            GdipFillRectangle, GdipGetPenBrushFill, GdipSetInterpolationMode, GdipSetSmoothingMode,
            GdiplusShutdown, GdiplusStartup, GdiplusStartupInput, GpBitmap, GpBrush, GpGraphics,
            GpImage, GpPath, GpPen, InterpolationModeHighQualityBicubic, SmoothingModeAntiAlias,
            Unit,
        },
    },
    UI::{
        Input::KeyboardAndMouse::SetFocus,
        WindowsAndMessaging::{
            DrawIconEx, GetCursorPos, ShowWindow, UpdateLayeredWindow, DI_NORMAL, SW_HIDE, SW_SHOW,
            ULW_ALPHA,
        },
    },
};

pub const BG_DARK_COLOR: u32 = 0x4c4c4c;
pub const FG_DARK_COLOR: u32 = 0x3b3b3b;
pub const BG_LIGHT_COLOR: u32 = 0xe0e0e0;
pub const FG_LIGHT_COLOR: u32 = 0xf2f2f2;
pub const ALPHA_MASK: u32 = 0xff000000;
pub const ICON_SIZE: i32 = 64;
pub const WINDOW_BORDER_SIZE: i32 = 10;
pub const ICON_BORDER_SIZE: i32 = 4;
pub const SCALE_FACTOR: i32 = 6;
pub const PREVIEW_CARD_GAP: i32 = 12;
pub const PREVIEW_CARD_MAX_WIDTH: i32 = 220;
pub const PREVIEW_CARD_CONTENT_PADDING: i32 = 10;
pub const PREVIEW_CARD_ASPECT_WIDTH: i32 = 16;
pub const PREVIEW_CARD_ASPECT_HEIGHT: i32 = 10;

// GDI Antialiasing Painter
pub struct GdiAAPainter {
    token: usize,
    hwnd: HWND,
    hdc_screen: HDC,
    rounded_corner: bool,
    show: bool,
}

impl GdiAAPainter {
    pub fn new(hwnd: HWND) -> Result<Self> {
        let startup_input = GdiplusStartupInput {
            GdiplusVersion: 1,
            ..Default::default()
        };
        let mut token: usize = 0;
        check_error(|| unsafe { GdiplusStartup(&mut token, &startup_input, std::ptr::null_mut()) })
            .context("Failed to initialize GDI+")?;

        let hdc_screen = unsafe { GetDC(Some(hwnd)) };
        let rounded_corner = is_win11();

        Ok(Self {
            token,
            hwnd,
            hdc_screen,
            rounded_corner,
            show: false,
        })
    }

    pub fn paint(&mut self, state: &SwitchAppsState) {
        let layout = OverlayLayout::for_state(state);

        let corner_radius = if self.rounded_corner {
            layout.overlay_corner_radius
        } else {
            0
        };

        let hwnd = self.hwnd;
        let hdc_screen = self.hdc_screen;

        let (fg_color, bg_color) = theme_color(is_light_theme());

        unsafe {
            let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
            let bitmap_mem = CreateCompatibleBitmap(hdc_screen, layout.width, layout.height);
            SelectObject(hdc_mem, bitmap_mem.into());

            let mut graphics = GpGraphics::default();
            let mut graphics_ptr: *mut GpGraphics = &mut graphics;
            GdipCreateFromHDC(hdc_mem, &mut graphics_ptr as _);
            GdipSetSmoothingMode(graphics_ptr, SmoothingModeAntiAlias);
            GdipSetInterpolationMode(graphics_ptr, InterpolationModeHighQualityBicubic);

            let mut bg_pen = GpPen::default();
            let mut bg_pen_ptr: *mut GpPen = &mut bg_pen;
            GdipCreatePen1(ALPHA_MASK | bg_color, 0.0, Unit(0), &mut bg_pen_ptr as _);

            let mut bg_brush = GpBrush::default();
            let mut bg_brush_ptr: *mut GpBrush = &mut bg_brush;
            GdipGetPenBrushFill(bg_pen_ptr, &mut bg_brush_ptr as _);

            if self.rounded_corner {
                draw_round_rect(
                    graphics_ptr,
                    bg_brush_ptr,
                    0.0,
                    0.0,
                    layout.width as f32,
                    layout.height as f32,
                    corner_radius as f32,
                );
            } else {
                GdipFillRectangle(
                    graphics_ptr,
                    bg_brush_ptr,
                    0.0,
                    0.0,
                    layout.width as f32,
                    layout.height as f32,
                );
            }

            let bitmap_entries = draw_entries(state, &layout, hdc_screen, fg_color, bg_color);

            let mut bitmap = GpBitmap::default();
            let mut bitmap_ptr: *mut GpBitmap = &mut bitmap as _;
            GdipCreateBitmapFromHBITMAP(bitmap_entries, HPALETTE::default(), &mut bitmap_ptr as _);

            let image_ptr: *mut GpImage = bitmap_ptr as *mut GpImage;
            GdipDrawImageRect(
                graphics_ptr,
                image_ptr,
                layout.content_rect.left as f32,
                layout.content_rect.top as f32,
                rect_width(&layout.content_rect) as f32,
                rect_height(&layout.content_rect) as f32,
            );

            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as _,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as _,
                ..Default::default()
            };
            let _ = UpdateLayeredWindow(
                hwnd,
                Some(hdc_screen),
                Some(&POINT {
                    x: layout.x,
                    y: layout.y,
                }),
                Some(&SIZE {
                    cx: layout.width,
                    cy: layout.height,
                }),
                Some(hdc_mem),
                Some(&POINT::default()),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );

            GdipDisposeImage(image_ptr);
            GdipDeleteBrush(bg_brush_ptr);
            GdipDeletePen(bg_pen_ptr);
            GdipDeleteGraphics(graphics_ptr);

            let _ = DeleteObject(bitmap_entries.into());
            let _ = DeleteObject(bitmap_mem.into());
            let _ = DeleteDC(hdc_mem);
        }

        if self.show {
            return;
        }
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            let _ = SetFocus(Some(self.hwnd));
        }
        self.show = true;
    }

    pub fn unpaint(&mut self, _state: SwitchAppsState) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.show = false;
    }
}

impl Drop for GdiAAPainter {
    fn drop(&mut self) {
        unsafe {
            ReleaseDC(Some(self.hwnd), self.hdc_screen);
            GdiplusShutdown(self.token);
        }
    }
}

pub fn find_clicked_app_index(state: &SwitchAppsState) -> Option<usize> {
    let layout = OverlayLayout::for_state(state);

    let mut cursor_pos = POINT::default();
    let _ = unsafe { GetCursorPos(&mut cursor_pos) };

    let xpos = cursor_pos.x - layout.x;
    let ypos = cursor_pos.y - layout.y;

    hit_test_app_index(&layout, xpos, ypos)
}

fn hit_test_app_index(layout: &OverlayLayout, xpos: i32, ypos: i32) -> Option<usize> {
    layout
        .entries
        .iter()
        .enumerate()
        .find(|(_, entry)| point_in_rect(&entry.card_rect, xpos, ypos))
        .map(|(index, _)| index)
}

const fn theme_color(light_theme: bool) -> (u32, u32) {
    match light_theme {
        true => (FG_LIGHT_COLOR, BG_LIGHT_COLOR),
        false => (FG_DARK_COLOR, BG_DARK_COLOR),
    }
}

unsafe fn draw_round_rect(
    graphic_ptr: *mut GpGraphics,
    brush_ptr: *mut GpBrush,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    corner_radius: f32,
) {
    unsafe {
        let mut path = GpPath::default();
        let mut path_ptr: *mut GpPath = &mut path;
        GdipCreatePath(FillModeAlternate, &mut path_ptr as _);
        GdipAddPathArc(
            path_ptr,
            left,
            top,
            corner_radius,
            corner_radius,
            180.0,
            90.0,
        );
        GdipAddPathArc(
            path_ptr,
            right - corner_radius,
            top,
            corner_radius,
            corner_radius,
            270.0,
            90.0,
        );
        GdipAddPathArc(
            path_ptr,
            right - corner_radius,
            bottom - corner_radius,
            corner_radius,
            corner_radius,
            0.0,
            90.0,
        );
        GdipAddPathArc(
            path_ptr,
            left,
            bottom - corner_radius,
            corner_radius,
            corner_radius,
            90.0,
            90.0,
        );
        GdipClosePathFigure(path_ptr);
        GdipFillPath(graphic_ptr, brush_ptr, path_ptr);
        GdipDeletePath(path_ptr);
    }
}

fn draw_entries(
    state: &SwitchAppsState,
    layout: &OverlayLayout,
    hdc_screen: HDC,
    fg_color: u32,
    bg_color: u32,
) -> HBITMAP {
    let width = rect_width(&layout.content_rect);
    let height = rect_height(&layout.content_rect);
    let scaled_width = width * SCALE_FACTOR;
    let scaled_height = height * SCALE_FACTOR;
    let scaled_card_corner_radius = layout.card_corner_radius * SCALE_FACTOR;
    let card_color = blend_color(bg_color, fg_color, 1, 2);

    unsafe {
        let hdc_tmp = CreateCompatibleDC(Some(hdc_screen));
        let bitmap_tmp = CreateCompatibleBitmap(hdc_screen, width, height);
        SelectObject(hdc_tmp, bitmap_tmp.into());

        let hdc_scaled = CreateCompatibleDC(Some(hdc_screen));
        let bitmap_scaled = CreateCompatibleBitmap(hdc_screen, scaled_width, scaled_height);
        SelectObject(hdc_scaled, bitmap_scaled.into());

        let fg_brush = CreateSolidBrush(COLORREF(fg_color));
        let bg_brush = CreateSolidBrush(COLORREF(bg_color));
        let card_brush = CreateSolidBrush(COLORREF(card_color));

        let rect = RECT {
            left: 0,
            top: 0,
            right: scaled_width,
            bottom: scaled_height,
        };

        FillRect(hdc_scaled, &rect, bg_brush);

        if state.render_mode == SwitchAppsRenderMode::Preview {
            for (i, _) in state.apps.iter().enumerate() {
                let entry = &layout.entries[i];
                let card_rect = scale_rect(
                    offset_rect(
                        entry.card_rect,
                        -layout.content_rect.left,
                        -layout.content_rect.top,
                    ),
                    SCALE_FACTOR,
                );
                fill_round_rect_region(
                    hdc_scaled,
                    if i == state.index {
                        fg_brush
                    } else {
                        card_brush
                    },
                    &card_rect,
                    scaled_card_corner_radius,
                );
            }
        } else if let Some(entry) = layout.entries.get(state.index) {
            let card_rect = scale_rect(
                offset_rect(
                    entry.card_rect,
                    -layout.content_rect.left,
                    -layout.content_rect.top,
                ),
                SCALE_FACTOR,
            );
            fill_round_rect_region(hdc_scaled, fg_brush, &card_rect, scaled_card_corner_radius);
        }

        for (i, app) in state.apps.iter().enumerate() {
            let icon_rect = scale_rect(
                offset_rect(
                    layout.entries[i].icon_rect,
                    -layout.content_rect.left,
                    -layout.content_rect.top,
                ),
                SCALE_FACTOR,
            );
            let _ = DrawIconEx(
                hdc_scaled,
                icon_rect.left,
                icon_rect.top,
                app.icon,
                rect_width(&icon_rect),
                rect_height(&icon_rect),
                0,
                None,
                DI_NORMAL,
            );
        }

        SetStretchBltMode(hdc_tmp, HALFTONE);
        let _ = StretchBlt(
            hdc_tmp,
            0,
            0,
            width,
            height,
            Some(hdc_scaled),
            0,
            0,
            scaled_width,
            scaled_height,
            SRCCOPY,
        );

        let _ = DeleteObject(fg_brush.into());
        let _ = DeleteObject(bg_brush.into());
        let _ = DeleteObject(card_brush.into());
        let _ = DeleteObject(bitmap_scaled.into());
        let _ = DeleteDC(hdc_scaled);
        let _ = DeleteDC(hdc_tmp);

        bitmap_tmp
    }
}

#[derive(Debug, Clone, Copy)]
struct OverlayEntryLayout {
    card_rect: RECT,
    icon_rect: RECT,
}

#[derive(Debug, Clone)]
struct OverlayLayout {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    content_rect: RECT,
    overlay_corner_radius: i32,
    card_corner_radius: i32,
    entries: Vec<OverlayEntryLayout>,
}

impl OverlayLayout {
    fn for_state(state: &SwitchAppsState) -> Self {
        Self::new(state.render_mode, state.apps.len(), get_moinitor_rect())
    }

    fn new(render_mode: SwitchAppsRenderMode, num_apps: usize, monitor_rect: RECT) -> Self {
        let monitor_width = monitor_rect.right - monitor_rect.left;
        let monitor_height = monitor_rect.bottom - monitor_rect.top;
        let outer_padding = WINDOW_BORDER_SIZE;

        if num_apps == 0 {
            return Self {
                x: monitor_rect.left + monitor_width / 2,
                y: monitor_rect.top + monitor_height / 2,
                width: outer_padding * 2,
                height: outer_padding * 2,
                content_rect: RECT {
                    left: outer_padding,
                    top: outer_padding,
                    right: outer_padding,
                    bottom: outer_padding,
                },
                overlay_corner_radius: 0,
                card_corner_radius: 0,
                entries: vec![],
            };
        }

        let gap = match render_mode {
            SwitchAppsRenderMode::IconOnly => 0,
            SwitchAppsRenderMode::Preview => PREVIEW_CARD_GAP,
        };
        let available_width =
            (monitor_width - outer_padding * 2 - gap * (num_apps as i32 - 1)).max(num_apps as i32);
        let card_width = match render_mode {
            SwitchAppsRenderMode::IconOnly => {
                ((available_width / num_apps as i32) - ICON_BORDER_SIZE * 2).min(ICON_SIZE)
                    + ICON_BORDER_SIZE * 2
            }
            SwitchAppsRenderMode::Preview => {
                (available_width / num_apps as i32).min(PREVIEW_CARD_MAX_WIDTH)
            }
        };
        let card_height = match render_mode {
            SwitchAppsRenderMode::IconOnly => card_width,
            SwitchAppsRenderMode::Preview => {
                (card_width * PREVIEW_CARD_ASPECT_HEIGHT) / PREVIEW_CARD_ASPECT_WIDTH
            }
        };
        let content_width = card_width * num_apps as i32 + gap * (num_apps as i32 - 1);
        let content_height = card_height;
        let width = content_width + outer_padding * 2;
        let height = content_height + outer_padding * 2;
        let x = monitor_rect.left + (monitor_width - width) / 2;
        let y = monitor_rect.top + (monitor_height - height) / 2;
        let content_rect = RECT {
            left: outer_padding,
            top: outer_padding,
            right: outer_padding + content_width,
            bottom: outer_padding + content_height,
        };
        let icon_size = match render_mode {
            SwitchAppsRenderMode::IconOnly => card_width - ICON_BORDER_SIZE * 2,
            SwitchAppsRenderMode::Preview => ICON_SIZE.min(
                (card_height - PREVIEW_CARD_CONTENT_PADDING * 2)
                    .min(card_width - PREVIEW_CARD_CONTENT_PADDING * 2),
            ),
        };
        let entries = (0..num_apps)
            .map(|index| {
                let left = content_rect.left + index as i32 * (card_width + gap);
                let top = content_rect.top;
                let card_rect = RECT {
                    left,
                    top,
                    right: left + card_width,
                    bottom: top + card_height,
                };
                OverlayEntryLayout {
                    card_rect,
                    icon_rect: centered_rect(card_rect, icon_size, icon_size),
                }
            })
            .collect();

        Self {
            x,
            y,
            width,
            height,
            content_rect,
            overlay_corner_radius: match render_mode {
                SwitchAppsRenderMode::IconOnly => card_height / 4,
                SwitchAppsRenderMode::Preview => (card_height / 6).max(10),
            },
            card_corner_radius: match render_mode {
                SwitchAppsRenderMode::IconOnly => card_height / 4,
                SwitchAppsRenderMode::Preview => (card_height / 8).max(8),
            },
            entries,
        }
    }
}

fn fill_round_rect_region(
    hdc: HDC,
    brush: windows::Win32::Graphics::Gdi::HBRUSH,
    rect: &RECT,
    radius: i32,
) {
    unsafe {
        let rgn = CreateRoundRectRgn(rect.left, rect.top, rect.right, rect.bottom, radius, radius);
        let _ = FillRgn(hdc, rgn, brush);
        let _ = DeleteObject(rgn.into());
    }
}

fn centered_rect(rect: RECT, width: i32, height: i32) -> RECT {
    let left = rect.left + (rect_width(&rect) - width) / 2;
    let top = rect.top + (rect_height(&rect) - height) / 2;
    RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

fn offset_rect(rect: RECT, dx: i32, dy: i32) -> RECT {
    RECT {
        left: rect.left + dx,
        top: rect.top + dy,
        right: rect.right + dx,
        bottom: rect.bottom + dy,
    }
}

fn scale_rect(rect: RECT, scale: i32) -> RECT {
    RECT {
        left: rect.left * scale,
        top: rect.top * scale,
        right: rect.right * scale,
        bottom: rect.bottom * scale,
    }
}

fn rect_width(rect: &RECT) -> i32 {
    rect.right - rect.left
}

fn rect_height(rect: &RECT) -> i32 {
    rect.bottom - rect.top
}

fn point_in_rect(rect: &RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

fn blend_color(start: u32, end: u32, numerator: u32, denominator: u32) -> u32 {
    fn blend_channel(start: u32, end: u32, numerator: u32, denominator: u32) -> u32 {
        ((start * (denominator - numerator)) + (end * numerator)) / denominator
    }

    let b = blend_channel(
        (start >> 16) & 0xff,
        (end >> 16) & 0xff,
        numerator,
        denominator,
    );
    let g = blend_channel(
        (start >> 8) & 0xff,
        (end >> 8) & 0xff,
        numerator,
        denominator,
    );
    let r = blend_channel(start & 0xff, end & 0xff, numerator, denominator);
    (b << 16) | (g << 8) | r
}

#[cfg(test)]
mod tests {
    use super::{hit_test_app_index, OverlayLayout, SwitchAppsRenderMode, WINDOW_BORDER_SIZE};
    use windows::Win32::Foundation::RECT;

    fn fake_monitor_rect(width: i32, height: i32) -> RECT {
        RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        }
    }

    #[test]
    fn hit_test_app_index_tracks_visual_slot_order_for_icon_mode() {
        let layout = OverlayLayout::new(
            SwitchAppsRenderMode::IconOnly,
            3,
            fake_monitor_rect(1920, 1080),
        );

        assert_eq!(
            hit_test_app_index(&layout, WINDOW_BORDER_SIZE + 5, WINDOW_BORDER_SIZE + 5),
            Some(0)
        );
        assert_eq!(
            hit_test_app_index(
                &layout,
                layout.entries[1].card_rect.left + 5,
                WINDOW_BORDER_SIZE + 5
            ),
            Some(1)
        );
        assert_eq!(
            hit_test_app_index(
                &layout,
                layout.entries[2].card_rect.left + 5,
                WINDOW_BORDER_SIZE + 5
            ),
            Some(2)
        );
    }

    #[test]
    fn hit_test_app_index_tracks_visual_slot_order_for_preview_mode() {
        let layout = OverlayLayout::new(
            SwitchAppsRenderMode::Preview,
            3,
            fake_monitor_rect(1920, 1080),
        );

        assert_eq!(
            hit_test_app_index(
                &layout,
                layout.entries[0].card_rect.left + 5,
                layout.entries[0].card_rect.top + 5
            ),
            Some(0)
        );
        assert_eq!(
            hit_test_app_index(
                &layout,
                layout.entries[1].card_rect.left + 5,
                layout.entries[1].card_rect.top + 5
            ),
            Some(1)
        );
        assert_eq!(
            hit_test_app_index(
                &layout,
                layout.entries[2].card_rect.left + 5,
                layout.entries[2].card_rect.top + 5
            ),
            Some(2)
        );
    }

    #[test]
    fn hit_test_app_index_rejects_border_and_outside_points() {
        let layout = OverlayLayout::new(
            SwitchAppsRenderMode::Preview,
            3,
            fake_monitor_rect(1920, 1080),
        );

        assert_eq!(
            hit_test_app_index(&layout, WINDOW_BORDER_SIZE - 1, WINDOW_BORDER_SIZE + 5),
            None
        );
        assert_eq!(
            hit_test_app_index(&layout, WINDOW_BORDER_SIZE + 5, WINDOW_BORDER_SIZE - 1),
            None
        );
        assert_eq!(
            hit_test_app_index(
                &layout,
                layout.content_rect.right,
                layout.entries[0].card_rect.top + 5
            ),
            None
        );
    }

    #[test]
    fn preview_layout_uses_wider_cards_and_fits_monitor() {
        let icon_layout = OverlayLayout::new(
            SwitchAppsRenderMode::IconOnly,
            5,
            fake_monitor_rect(1920, 1080),
        );
        let preview_layout = OverlayLayout::new(
            SwitchAppsRenderMode::Preview,
            5,
            fake_monitor_rect(1920, 1080),
        );

        assert!(preview_layout.width <= 1920);
        assert!(preview_layout.height <= 1080);
        assert!(
            (preview_layout.entries[0].card_rect.right - preview_layout.entries[0].card_rect.left)
                > (icon_layout.entries[0].card_rect.right - icon_layout.entries[0].card_rect.left)
        );
        assert!(
            (preview_layout.entries[0].card_rect.right - preview_layout.entries[0].card_rect.left)
                > (preview_layout.entries[0].card_rect.bottom
                    - preview_layout.entries[0].card_rect.top)
        );
    }
}
