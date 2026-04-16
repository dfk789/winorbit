use crate::config::SwitchAppsRenderMode;
use crate::preview::AppPreview;
use crate::switch_apps::SwitchAppsState;
use crate::utils::{check_error, get_moinitor_rect, is_light_theme, is_win11};

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use windows::Win32::{
    Foundation::{COLORREF, HWND, POINT, RECT, SIZE},
    Graphics::{
        Dwm::{
            DwmQueryThumbnailSourceSize, DwmRegisterThumbnail, DwmUnregisterThumbnail,
            DwmUpdateThumbnailProperties, DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY,
            DWM_TNP_RECTDESTINATION, DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE,
        },
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
    preview_thumbnails: HashMap<isize, RegisteredThumbnail>,
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
            preview_thumbnails: HashMap::new(),
            show: false,
        })
    }

    pub fn paint(&mut self, state: &SwitchAppsState) {
        let layout = OverlayLayout::for_state(state);
        let live_previews = self.prepare_live_previews(state, &layout);

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

            let bitmap_entries = draw_entries(
                state,
                &layout,
                &live_previews,
                hdc_screen,
                fg_color,
                bg_color,
            );

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
        self.preview_thumbnails.clear();
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.show = false;
    }

    fn prepare_live_previews(
        &mut self,
        state: &SwitchAppsState,
        layout: &OverlayLayout,
    ) -> Vec<bool> {
        let mut live_previews = vec![false; state.apps.len()];
        if state.render_mode != SwitchAppsRenderMode::Preview {
            self.preview_thumbnails.clear();
            return live_previews;
        }

        let mut keep_keys = HashSet::new();
        let mut failed_keys = HashSet::new();

        for (index, app) in state.apps.iter().enumerate() {
            let AppPreview::DwmThumbnail(preview) = app.preview else {
                continue;
            };

            let key = preview.source_hwnd.0 as isize;
            keep_keys.insert(key);

            if !self.preview_thumbnails.contains_key(&key) {
                match RegisteredThumbnail::register(self.hwnd, preview.source_hwnd) {
                    Ok(thumbnail) => {
                        self.preview_thumbnails.insert(key, thumbnail);
                    }
                    Err(err) => {
                        debug!(
                            "preview register failed for hwnd {:?}: {err}",
                            preview.source_hwnd
                        );
                        failed_keys.insert(key);
                        continue;
                    }
                }
            }

            let Some(thumbnail) = self.preview_thumbnails.get(&key) else {
                continue;
            };

            let source_size = match thumbnail.source_size() {
                Ok(size) => size,
                Err(err) => {
                    debug!(
                        "preview source-size query failed for hwnd {:?}: {err}",
                        preview.source_hwnd
                    );
                    failed_keys.insert(key);
                    continue;
                }
            };

            let Some(destination_rect) =
                fit_preview_destination(layout.entries[index].preview_rect, source_size)
            else {
                debug!(
                    "preview destination invalid for hwnd {:?}: {:?}",
                    preview.source_hwnd, layout.entries[index].preview_rect
                );
                failed_keys.insert(key);
                continue;
            };

            if let Err(err) = thumbnail.show(destination_rect) {
                debug!(
                    "preview property update failed for hwnd {:?}: {err}",
                    preview.source_hwnd
                );
                failed_keys.insert(key);
                continue;
            }

            live_previews[index] = true;
        }

        self.preview_thumbnails
            .retain(|key, _| keep_keys.contains(key) && !failed_keys.contains(key));

        live_previews
    }
}

impl Drop for GdiAAPainter {
    fn drop(&mut self) {
        self.preview_thumbnails.clear();
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
    live_previews: &[bool],
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
            if live_previews.get(i).copied().unwrap_or(false) {
                continue;
            }
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

#[derive(Debug)]
struct RegisteredThumbnail {
    source_hwnd: HWND,
    handle: isize,
}

impl RegisteredThumbnail {
    fn register(destination_hwnd: HWND, source_hwnd: HWND) -> Result<Self> {
        let handle = unsafe {
            // The returned thumbnail handle is owned by this struct and released in Drop.
            DwmRegisterThumbnail(destination_hwnd, source_hwnd)
        }
        .with_context(|| format!("failed to register DWM thumbnail for {source_hwnd:?}"))?;

        Ok(Self {
            source_hwnd,
            handle,
        })
    }

    fn source_size(&self) -> Result<SIZE> {
        unsafe { DwmQueryThumbnailSourceSize(self.handle) }.with_context(|| {
            format!(
                "failed to query DWM thumbnail source size for {:?}",
                self.source_hwnd
            )
        })
    }

    fn show(&self, destination_rect: RECT) -> Result<()> {
        let properties = DWM_THUMBNAIL_PROPERTIES {
            dwFlags: DWM_TNP_RECTDESTINATION
                | DWM_TNP_OPACITY
                | DWM_TNP_VISIBLE
                | DWM_TNP_SOURCECLIENTAREAONLY,
            rcDestination: destination_rect,
            opacity: 255,
            fVisible: true.into(),
            fSourceClientAreaOnly: false.into(),
            ..Default::default()
        };

        unsafe { DwmUpdateThumbnailProperties(self.handle, &properties) }.with_context(|| {
            format!(
                "failed to update DWM thumbnail properties for {:?}",
                self.source_hwnd
            )
        })
    }
}

impl Drop for RegisteredThumbnail {
    fn drop(&mut self) {
        if let Err(err) = unsafe { DwmUnregisterThumbnail(self.handle) } {
            debug!(
                "preview unregister failed for hwnd {:?}: {err}",
                self.source_hwnd
            );
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct OverlayEntryLayout {
    card_rect: RECT,
    preview_rect: RECT,
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
                let preview_padding = match render_mode {
                    SwitchAppsRenderMode::IconOnly => ICON_BORDER_SIZE,
                    SwitchAppsRenderMode::Preview => PREVIEW_CARD_CONTENT_PADDING
                        .min((card_width - 1).max(0) / 2)
                        .min((card_height - 1).max(0) / 2),
                };
                let preview_rect = inset_rect(card_rect, preview_padding);
                let icon_size = ICON_SIZE
                    .min(rect_width(&preview_rect))
                    .min(rect_height(&preview_rect));
                OverlayEntryLayout {
                    card_rect,
                    preview_rect,
                    icon_rect: centered_rect(preview_rect, icon_size, icon_size),
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

fn inset_rect(rect: RECT, padding: i32) -> RECT {
    RECT {
        left: rect.left + padding,
        top: rect.top + padding,
        right: rect.right - padding,
        bottom: rect.bottom - padding,
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

fn fit_preview_destination(bounds: RECT, source_size: SIZE) -> Option<RECT> {
    let bounds_width = rect_width(&bounds);
    let bounds_height = rect_height(&bounds);
    if bounds_width <= 0 || bounds_height <= 0 || source_size.cx <= 0 || source_size.cy <= 0 {
        return None;
    }

    let bounds_width = bounds_width as i64;
    let bounds_height = bounds_height as i64;
    let source_width = source_size.cx as i64;
    let source_height = source_size.cy as i64;

    let (width, height) = if bounds_width * source_height <= bounds_height * source_width {
        let width = bounds_width as i32;
        let height = ((bounds_width * source_height) / source_width).max(1) as i32;
        (width, height)
    } else {
        let width = ((bounds_height * source_width) / source_height).max(1) as i32;
        let height = bounds_height as i32;
        (width, height)
    };

    Some(centered_rect(bounds, width, height))
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
    use super::{
        fit_preview_destination, hit_test_app_index, rect_height, rect_width, OverlayLayout,
        SwitchAppsRenderMode, WINDOW_BORDER_SIZE,
    };
    use windows::Win32::Foundation::{RECT, SIZE};

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
        assert!(
            preview_layout.entries[0].preview_rect.left > preview_layout.entries[0].card_rect.left
        );
        assert!(
            preview_layout.entries[0].icon_rect.left >= preview_layout.entries[0].preview_rect.left
        );
        assert!(
            preview_layout.entries[0].icon_rect.right
                <= preview_layout.entries[0].preview_rect.right
        );
    }

    #[test]
    fn fit_preview_destination_preserves_wide_source_aspect_ratio() {
        let destination = fit_preview_destination(
            RECT {
                left: 10,
                top: 20,
                right: 210,
                bottom: 140,
            },
            SIZE { cx: 1600, cy: 900 },
        )
        .expect("destination rect should be calculated");

        assert_eq!(rect_width(&destination), 200);
        assert_eq!(rect_height(&destination), 112);
        assert_eq!(destination.top, 24);
        assert_eq!(destination.bottom, 136);
    }

    #[test]
    fn fit_preview_destination_preserves_tall_source_aspect_ratio() {
        let destination = fit_preview_destination(
            RECT {
                left: 10,
                top: 20,
                right: 130,
                bottom: 220,
            },
            SIZE { cx: 900, cy: 1600 },
        )
        .expect("destination rect should be calculated");

        assert_eq!(rect_width(&destination), 112);
        assert_eq!(rect_height(&destination), 200);
        assert_eq!(destination.left, 14);
        assert_eq!(destination.right, 126);
    }

    #[test]
    fn fit_preview_destination_rejects_zero_sized_inputs() {
        assert_eq!(
            fit_preview_destination(
                RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 100,
                },
                SIZE { cx: 800, cy: 600 },
            ),
            None
        );
        assert_eq!(
            fit_preview_destination(
                RECT {
                    left: 0,
                    top: 0,
                    right: 100,
                    bottom: 100,
                },
                SIZE { cx: 0, cy: 600 },
            ),
            None
        );
    }
}
