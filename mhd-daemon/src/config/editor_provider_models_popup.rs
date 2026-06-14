//! Models list editor modal popup for a provider.
//!
//! Opens a small modal dialog showing a scrollable list of model ID
//! strings.  Each row can be inline-edited, deleted, and new models
//! can be added.  Follows the same pattern as `editor_provider_popup`.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use crate::app::DaemonControl;
use crate::config::editor_layout::*;
use crate::config::editor_state::ButtonStyle;
use crate::config::editor_theme::{
    draw_button, draw_rounded_border_in_buffer, draw_rounded_rect_in_buffer, to_utf16_z,
};
use crate::core::native_theme::NativeTheme;

// ── Constants ────────────────────────────────────────────────────────

const POPUP_WIDTH_BASE: i32 = 460;
const POPUP_HEIGHT_BASE: i32 = 420;
const POPUP_HEADER_HEIGHT_BASE: i32 = 48;
const POPUP_FOOTER_HEIGHT_BASE: i32 = 52;
const POPUP_RADIUS_BASE: f32 = 12.0;
const POPUP_PADDING: i32 = 20;
const POPUP_FIELD_HEIGHT: i32 = 30;
const POPUP_ROW_HEIGHT: i32 = 32;

/// Custom message for per-model test results.
/// `WPARAM` = model index, `LPARAM` = pointer to a heap-allocated bool (true=success).
const WM_MODEL_TEST_RESULT: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 3;

/// Custom message for fetch-models completion.
/// `LPARAM` = pointer to a heap-allocated `Vec<String>` (or null on failure/cancellation).
const WM_FETCH_MODELS_RESULT: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 4;

// ── Hit targets ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelsPopupHit {
    None,
    /// Click on a model row field to start inline editing.
    ModelField(usize),
    /// Checkbox to toggle model selection.
    ModelCheckbox(usize),
    /// Test button on a model row.
    ModelTestBtn(usize),
    /// Delete button on a model row.
    ModelDelete(usize),
    /// "+ Add Model" button.
    AddBtn,
    SelectAllBtn,
    DeselectAllBtn,
    /// Scrollbar (the hit area is the list's right edge).
    Scrollbar,
    FetchModelsBtn,
    SaveBtn,
    CancelBtn,
}

// ── Popup state ──────────────────────────────────────────────────────

struct ModelsPopupState {
    hwnd: HWND,
    _parent_hwnd: HWND,

    /// The live-edited model list.
    models: Vec<String>,

    /// Endpoint URL for testing models.
    endpoint: String,
    /// API key for testing models.
    api_key: String,

    // Inline editing state for a single row at a time
    editing_idx: Option<usize>,
    edit_text: String,
    edit_cursor: usize,
    edit_old_value: String,

    /// Per-model selection state (checked = will be saved).
    model_selected: Vec<bool>,
    /// Per-model test status: None=untested, Some(true)=success, Some(false)=failure.
    model_test_results: Vec<Option<bool>>,
    /// Index of model currently being tested.
    testing_idx: Option<usize>,
    /// True while a background fetch-models request is in flight.
    fetching_models: bool,
    /// Cancellation flag for the background test thread.
    cancelled: Arc<AtomicBool>,

    // Scrolling state
    content_scroll_y: i32,
    list_area_h: i32, // available height for the list

    // UI state
    theme: NativeTheme,
    scale: f32,
    win_w: i32,
    win_h: i32,
    hovered_target: ModelsPopupHit,
    /// True when dragging the scrollbar thumb.
    is_dragging_scroll: bool,
    scroll_drag_start_y: i32,
    scroll_drag_start_offset: i32,
    should_close: bool,
    saved: bool,
}

// ── Window registration ──────────────────────────────────────────────

static POPUP_CLASS: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

fn ensure_popup_class() -> u16 {
    *POPUP_CLASS.get_or_init(|| {
        let hinst: HINSTANCE = unsafe {
            HINSTANCE(
                windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                    .unwrap_or_default()
                    .0,
            )
        };
        let class_name = to_utf16_z("mhd_ModelsEditorPopup");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(models_popup_wndproc),
            cbClsExtra: 0,
            cbWndExtra: std::mem::size_of::<isize>() as i32,
            hInstance: hinst,
            hIcon: HICON::default(),
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
            hbrBackground: HBRUSH::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
        };
        unsafe { RegisterClassW(&wc) }
    })
}

// ── Open popup ───────────────────────────────────────────────────────

/// Open the models list editor as a modal dialog.
/// `models` is the initial list of model IDs.
/// Returns `Some(updated_list)` if saved, or `None` if cancelled.
pub fn open_models_popup(
    parent_hwnd: HWND,
    theme: &NativeTheme,
    scale: f32,
    models: Vec<String>,
    endpoint: &str,
    api_key: &str,
    handle: &crate::app::AppHandle,
) -> Option<Vec<String>> {
    let win_w = (POPUP_WIDTH_BASE as f32 * scale) as i32;
    let win_h = (POPUP_HEIGHT_BASE as f32 * scale) as i32;

    // Center on parent window
    let mut parent_rc = RECT::default();
    unsafe {
        let _ = GetWindowRect(parent_hwnd, &mut parent_rc);
    }
    let cx = parent_rc.left + (parent_rc.right - parent_rc.left - win_w) / 2;
    let cy = parent_rc.top + (parent_rc.bottom - parent_rc.top - win_h) / 2;

    let header_h = (POPUP_HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let footer_h = (POPUP_FOOTER_HEIGHT_BASE as f32 * scale) as i32;
    let pad = (POPUP_PADDING as f32 * scale) as i32;
    let list_area_h = win_h - header_h - footer_h - pad;

    let model_test_results = (0..models.len()).map(|_| None).collect();
    let model_selected = (0..models.len()).map(|_| true).collect();

    let popup_state = Box::new(ModelsPopupState {
        hwnd: HWND::default(),
        _parent_hwnd: parent_hwnd,
        models,
        model_selected,
        endpoint: endpoint.to_string(),
        api_key: api_key.to_string(),
        editing_idx: None,
        edit_text: String::new(),
        edit_cursor: 0,
        edit_old_value: String::new(),
        model_test_results,
        testing_idx: None,
        fetching_models: false,
        cancelled: Arc::new(AtomicBool::new(false)),
        content_scroll_y: 0,
        list_area_h,
        theme: theme.clone(),
        scale,
        win_w,
        win_h,
        hovered_target: ModelsPopupHit::None,
        is_dragging_scroll: false,
        scroll_drag_start_y: 0,
        scroll_drag_start_offset: 0,
        should_close: false,
        saved: false,
    });
    let popup_ptr = Box::into_raw(popup_state);

    let class_atom = ensure_popup_class();
    let hinst: HINSTANCE = unsafe {
        HINSTANCE(
            windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                .unwrap_or_default()
                .0,
        )
    };

    let class_wz = to_utf16_z("#32770");
    let class_ptr = if class_atom != 0 {
        PCWSTR::from_raw(class_atom as *const u16)
    } else {
        PCWSTR::from_raw(class_wz.as_ptr())
    };

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class_ptr,
            PCWSTR::from_raw(to_utf16_z("Edit Models").as_ptr()),
            WS_POPUP,
            0,
            0,
            win_w,
            win_h,
            None,
            None,
            hinst,
            Some(popup_ptr as *mut c_void),
        )
    }
    .ok();

    let hwnd = match hwnd {
        Some(h) => h,
        None => {
            let _ = unsafe { Box::from_raw(popup_ptr) };
            return None;
        }
    };

    unsafe {
        (*popup_ptr).hwnd = hwnd;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, popup_ptr as isize);
    }

    // Disable parent window
    unsafe {
        let _ = EnableWindow(parent_hwnd, false);
    }

    // Position and show
    unsafe {
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, cx, cy, win_w, win_h, SWP_SHOWWINDOW);
    }

    // Initial paint
    unsafe {
        paint_models_popup(hwnd, popup_ptr);
    }

    // Modal message loop
    let mut daemon_shutdown = false;
    unsafe {
        loop {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    daemon_shutdown = true;
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            if daemon_shutdown {
                break;
            }

            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if state_ptr == 0 {
                break;
            }
            let should_close = (*(state_ptr as *mut ModelsPopupState)).should_close;
            if should_close {
                break;
            }

            // Check if the daemon is shutting down
            if !handle.status() {
                daemon_shutdown = true;
                break;
            }

            let _ = MsgWaitForMultipleObjects(None, false, 200, QS_ALLINPUT);
        }
    }

    if daemon_shutdown {
        // The daemon is shutting down — don't bother with cleanup,
        // the process will exit.
        return None;
    }

    // Re-enable parent and bring it to front
    unsafe {
        let _ = EnableWindow(parent_hwnd, true);
        let _ = SetForegroundWindow(parent_hwnd);
    }

    let saved = unsafe { (*popup_ptr).saved };
    let result = if saved {
        unsafe { Some((*popup_ptr).models.clone()) }
    } else {
        None
    };

    // Cleanup — signal the test thread to stop
    unsafe {
        (*popup_ptr).cancelled.store(true, Ordering::SeqCst);
    }
    if !hwnd.is_invalid() {
        unsafe {
            DestroyWindow(hwnd).ok();
        }
    }
    unsafe {
        let _ = Box::from_raw(popup_ptr);
    }
    result
}

// ── Paint ────────────────────────────────────────────────────────────

unsafe fn paint_models_popup(hwnd: HWND, state_ptr: *mut ModelsPopupState) {
    unsafe {
        let state = &*state_ptr;
        let theme = &state.theme;
        let w = state.win_w;
        let h = state.win_h;
        let scale = state.scale;

        let mut frame = match crate::renderer::DibFrame::new(w, h) {
            Some(f) => f,
            None => return,
        };
        let dib_dc = frame.dc();
        let bits = frame.pixels_mut().as_mut_ptr() as *mut c_void;

        let popup_bg = theme.surface.blend_over(theme.background);
        let radius = (POPUP_RADIUS_BASE * scale) as i32;
        crate::osd::draw_rounded_rect(frame.pixels_mut(), w, h, radius, popup_bg);

        let _ = SetBkMode(dib_dc, TRANSPARENT);

        // Fonts
        let title_font = crate::renderer::create_font(
            -(FONT_TITLE_SIZE as f32 * scale) as i32,
            true,
            "Segoe UI Variable",
        );
        let body_font = crate::renderer::create_font(
            -(FONT_BODY_SIZE as f32 * scale) as i32,
            false,
            "Segoe UI Variable",
        );
        let small_font = crate::renderer::create_font(
            -(FONT_SMALL_SIZE as f32 * scale) as i32,
            false,
            "Segoe UI Variable",
        );
        let _old_font = SelectObject(dib_dc, title_font);

        // ── Header title ──────────────────────────────────────────
        SetTextColor(dib_dc, theme.text.to_colorref());
        let header_h = (POPUP_HEADER_HEIGHT_BASE as f32 * scale) as i32;
        let pad = (POPUP_PADDING as f32 * scale) as i32;
        let mut title_wz = to_utf16_z("Edit Models");
        let mut title_rc = RECT {
            left: pad,
            top: pad / 2,
            right: w - pad,
            bottom: pad / 2 + 18 + 4,
        };
        DrawTextW(
            dib_dc,
            &mut title_wz,
            &mut title_rc,
            DT_LEFT | DT_SINGLELINE,
        );

        // ── Fetch Models button (right-aligned in header) ────────────
        let fetch_btn_w = (100.0 * scale) as i32;
        let fetch_btn_h = (28.0 * scale) as i32;
        let fetch_btn_x = w - pad - fetch_btn_w;
        let fetch_btn_y = (header_h - fetch_btn_h) / 2;
        let is_fetch_hovered = state.hovered_target == ModelsPopupHit::FetchModelsBtn;
        let fetch_label = if state.fetching_models {
            "Getting.."
        } else {
            "Get Models"
        };
        draw_button(
            dib_dc,
            bits,
            w,
            h,
            fetch_btn_x,
            fetch_btn_y,
            fetch_btn_w,
            fetch_btn_h,
            fetch_label,
            theme,
            body_font,
            is_fetch_hovered,
            ButtonStyle::Secondary,
        );

        // Separator under header
        let sep_brush = CreateSolidBrush(theme.border.to_colorref());
        FillRect(
            dib_dc,
            &RECT {
                left: pad,
                top: header_h - 1,
                right: w - pad,
                bottom: header_h,
            },
            sep_brush,
        );

        // ── List area ──────────────────────────────────────────────
        let footer_h = (POPUP_FOOTER_HEIGHT_BASE as f32 * scale) as i32;
        let footer_y = h - footer_h;
        let list_y = header_h + pad;
        let list_h = state.list_area_h;
        let row_h = (POPUP_ROW_HEIGHT as f32 * scale) as i32;
        let field_h = (POPUP_FIELD_HEIGHT as f32 * scale) as i32;
        let del_w = (28.0 * scale) as i32;
        let scroll_y = state.content_scroll_y;

        // Total content height
        let total_rows = state.models.len() as i32;
        let add_btn_h = row_h;
        let content_h = total_rows * row_h + add_btn_h;

        // ── Clip to list area using SaveDC/RestoreDC ──────────────
        let saved_dc = SaveDC(dib_dc);
        let list_rect = RECT {
            left: pad,
            top: list_y,
            right: w - pad,
            bottom: list_y + list_h,
        };
        let clip_rgn = CreateRectRgn(
            list_rect.left,
            list_rect.top,
            list_rect.right,
            list_rect.bottom,
        );
        let _ = SelectClipRgn(dib_dc, clip_rgn);
        let _ = DeleteObject(clip_rgn); // DC uses a copy

        // ── Draw label row above the list ───────────────────────────
        let _ = SelectObject(dib_dc, small_font);
        SetTextColor(dib_dc, theme.text_muted.to_colorref());
        let label_y = list_y;
        let label_h = (16.0 * scale) as i32;
        let mut header_wz = to_utf16_z("Model ID");
        let mut header_rc = RECT {
            left: pad + (18.0 * scale) as i32,
            top: label_y,
            right: w - pad,
            bottom: label_y + label_h,
        };
        let _ = DrawTextW(
            dib_dc,
            &mut header_wz,
            &mut header_rc,
            DT_LEFT | DT_SINGLELINE,
        );

        // Select All / Deselect All buttons
        let sel_btn_w = (50.0 * scale) as i32;
        let sel_btn_h = label_h;
        let sel_btn_y = label_y;
        let deselect_x = w - pad - sel_btn_w;
        let select_x = deselect_x - (4.0 * scale) as i32 - sel_btn_w;

        let is_select_hovered = state.hovered_target == ModelsPopupHit::SelectAllBtn;
        let is_deselect_hovered = state.hovered_target == ModelsPopupHit::DeselectAllBtn;
        draw_button(
            dib_dc,
            bits,
            w,
            h,
            select_x,
            sel_btn_y,
            sel_btn_w,
            sel_btn_h,
            "All",
            theme,
            small_font,
            is_select_hovered,
            ButtonStyle::Secondary,
        );
        draw_button(
            dib_dc,
            bits,
            w,
            h,
            deselect_x,
            sel_btn_y,
            sel_btn_w,
            sel_btn_h,
            "None",
            theme,
            small_font,
            is_deselect_hovered,
            ButtonStyle::Secondary,
        );

        let list_content_top = label_y + label_h;
        let scrollbar_w = (8.0 * scale) as i32;
        let scrollbar_x = w - pad - scrollbar_w;

        // ── Model rows ───────────────────────────────────────────
        let test_btn_w = (50.0 * scale) as i32;
        let btn_gap = (4.0 * scale) as i32;

        let checkbox_w = (16.0 * scale) as i32;
        let checkbox_h = (16.0 * scale) as i32;

        for (i, model_id) in state.models.iter().enumerate() {
            let ry = list_content_top + i as i32 * row_h - scroll_y;
            let row_visible = ry + row_h > list_y && ry < list_y + list_h;

            if !row_visible {
                continue;
            }

            let is_editing = state.editing_idx == Some(i);
            let is_hovered = state.hovered_target == ModelsPopupHit::ModelField(i);

            // Row background (zebra)
            if i % 2 == 1 && !is_editing {
                let bg = theme.surface.blend_over(theme.background);
                draw_rounded_rect_in_buffer(
                    bits,
                    w,
                    h,
                    RECT {
                        left: pad,
                        top: ry,
                        right: w - pad - scrollbar_w,
                        bottom: ry + row_h,
                    },
                    0,
                    bg,
                );
            }

            // Button positions from the right edge
            let del_x = w - pad - scrollbar_w - del_w;
            let test_btn_x = del_x - btn_gap - test_btn_w;
            let field_right = test_btn_x - btn_gap;

            // Checkbox on the left
            let chk_x = pad;
            let chk_y = ry + (row_h - checkbox_h) / 2;
            let is_chk_hovered = state.hovered_target == ModelsPopupHit::ModelCheckbox(i);
            let is_checked = state.model_selected.get(i).copied().unwrap_or(true);
            let chk_bg = if is_checked {
                theme.accent
            } else if is_chk_hovered {
                theme
                    .hover
                    .blend_over(theme.surface.blend_over(theme.background))
            } else {
                theme.surface.blend_over(theme.background)
            };
            let chk_border = if is_chk_hovered {
                theme.text
            } else if is_checked {
                theme.accent
            } else {
                theme.border
            };
            draw_rounded_rect_in_buffer(
                bits,
                w,
                h,
                RECT {
                    left: chk_x,
                    top: chk_y,
                    right: chk_x + checkbox_w,
                    bottom: chk_y + checkbox_h,
                },
                (3.0 * scale) as i32,
                chk_bg,
            );
            draw_rounded_border_in_buffer(
                bits,
                w,
                h,
                RECT {
                    left: chk_x,
                    top: chk_y,
                    right: chk_x + checkbox_w,
                    bottom: chk_y + checkbox_h,
                },
                (3.0 * scale) as i32,
                1,
                chk_border,
            );
            if is_checked {
                // Draw a check mark character (✓)
                let mut chk_wz = to_utf16_z("\u{2713}");
                let mut chk_rc = RECT {
                    left: chk_x,
                    top: chk_y,
                    right: chk_x + checkbox_w,
                    bottom: chk_y + checkbox_h,
                };
                SetTextColor(dib_dc, theme.text.to_colorref());
                let _ = SelectObject(dib_dc, small_font);
                let _ = DrawTextW(
                    dib_dc,
                    &mut chk_wz,
                    &mut chk_rc,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
                SelectObject(dib_dc, body_font);
            }

            // Text field background + border (offset by checkbox)
            let field_left = chk_x + checkbox_w + (6.0 * scale) as i32;
            let bg = if is_editing {
                theme.surface.blend_over(theme.background)
            } else if is_hovered {
                theme
                    .hover
                    .blend_over(theme.surface.blend_over(theme.background))
            } else {
                theme.surface.blend_over(theme.background)
            };
            let border = if is_editing {
                theme.accent
            } else if is_hovered {
                theme.text
            } else {
                theme.border
            };

            let field_rect = RECT {
                left: field_left,
                top: ry + (row_h - field_h) / 2,
                right: field_right,
                bottom: ry + (row_h + field_h) / 2,
            };
            draw_rounded_rect_in_buffer(bits, w, h, field_rect, (4.0 * scale) as i32, bg);
            draw_rounded_border_in_buffer(bits, w, h, field_rect, (4.0 * scale) as i32, 1, border);

            // Draw text clipped to field interior
            let display_text = if is_editing {
                &state.edit_text
            } else {
                model_id
            };
            let mut text_rect = RECT {
                left: field_rect.left + (6.0 * scale) as i32,
                top: field_rect.top,
                right: field_rect.right - (6.0 * scale) as i32,
                bottom: field_rect.bottom,
            };
            SelectObject(dib_dc, body_font);
            SetTextColor(dib_dc, theme.text.to_colorref());
            let mut text_wz = to_utf16_z(display_text);
            let _ = DrawTextW(
                dib_dc,
                &mut text_wz,
                &mut text_rect,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );

            // Draw cursor if editing
            if is_editing && state.edit_cursor <= display_text.len() {
                let prefix = &display_text[..state.edit_cursor];
                let prefix_wz: Vec<u16> = prefix.encode_utf16().collect();
                let mut sz = SIZE::default();
                let _ = GetTextExtentPoint32W(dib_dc, &prefix_wz, &mut sz);
                let cursor_x = text_rect.left + sz.cx;
                let cursor_y = field_rect.top + (field_h - (12.0 * scale) as i32) / 2;
                let cursor_height = (12.0 * scale) as i32;
                let cursor_pen = CreatePen(PS_SOLID, 1, theme.text.to_colorref());
                let _ = SelectObject(dib_dc, cursor_pen);
                let _ = MoveToEx(dib_dc, cursor_x, cursor_y, None);
                let _ = LineTo(dib_dc, cursor_x, cursor_y + cursor_height);
                let _ = DeleteObject(cursor_pen);
            }

            // Test button (colored by test status)
            let is_testing = state.testing_idx == Some(i);
            let test_status = state.model_test_results.get(i).copied().unwrap_or(None);
            let is_test_hovered = state.hovered_target == ModelsPopupHit::ModelTestBtn(i);
            let test_label = if is_testing { ".." } else { "Test" };
            let test_btn_style = match test_status {
                Some(true) => ButtonStyle::Success,
                Some(false) => ButtonStyle::DangerGhost,
                None => ButtonStyle::Secondary, // neutral
            };
            let test_btn_h = (20.0 * scale) as i32;
            draw_button(
                dib_dc,
                bits,
                w,
                h,
                test_btn_x,
                ry + (row_h - test_btn_h) / 2,
                test_btn_w,
                test_btn_h,
                test_label,
                theme,
                body_font,
                is_test_hovered || is_testing,
                test_btn_style,
            );

            // Delete X button
            let is_del_hovered = state.hovered_target == ModelsPopupHit::ModelDelete(i);
            let del_btn_h = (20.0 * scale) as i32;
            draw_button(
                dib_dc,
                bits,
                w,
                h,
                del_x,
                ry + (row_h - del_btn_h) / 2,
                del_w,
                del_btn_h,
                "X",
                theme,
                body_font,
                is_del_hovered,
                ButtonStyle::DangerGhost,
            );
        }

        // ── Add Model button ───────────────────────────────────────
        let add_y = list_content_top + total_rows * row_h - scroll_y;
        let is_add_hovered = state.hovered_target == ModelsPopupHit::AddBtn;
        draw_button(
            dib_dc,
            bits,
            w,
            h,
            pad,
            add_y,
            (120.0 * scale) as i32,
            row_h - (4.0 * scale) as i32,
            "+ Add Model",
            theme,
            body_font,
            is_add_hovered,
            ButtonStyle::Secondary,
        );

        // ── Restore clip ────────────────────────────────────────────
        let _ = RestoreDC(dib_dc, saved_dc);

        // ── Scrollbar ─────────────────────────────────────────────
        if content_h > list_h {
            let thumb_h = (list_h as f32 * list_h as f32 / content_h as f32).max(20.0) as i32;
            let max_scroll = content_h - list_h;
            let thumb_y = if max_scroll > 0 {
                list_y + (scroll_y as f32 / max_scroll as f32 * (list_h - thumb_h) as f32) as i32
            } else {
                list_y
            };

            let track_rect = RECT {
                left: scrollbar_x,
                top: list_y,
                right: scrollbar_x + scrollbar_w,
                bottom: list_y + list_h,
            };
            draw_rounded_rect_in_buffer(
                bits,
                w,
                h,
                track_rect,
                0,
                theme.surface.blend_over(theme.background),
            );
            let thumb_rect = RECT {
                left: scrollbar_x,
                top: thumb_y,
                right: scrollbar_x + scrollbar_w,
                bottom: (thumb_y + thumb_h).min(list_y + list_h),
            };
            draw_rounded_rect_in_buffer(
                bits,
                w,
                h,
                thumb_rect,
                (scrollbar_w / 2).max(1),
                theme.text_muted,
            );
        }

        // ── Separator above footer ─────────────────────────────────
        let sep2_brush = CreateSolidBrush(theme.border.to_colorref());
        FillRect(
            dib_dc,
            &RECT {
                left: pad,
                top: footer_y,
                right: w - pad,
                bottom: footer_y + 1,
            },
            sep2_brush,
        );

        // ── Footer buttons ──────────────────────────────────────────
        let btn_h = (30.0 * scale) as i32;
        let btn_w = (90.0 * scale) as i32;
        let btn_y = footer_y + (footer_h - btn_h) / 2;
        let save_x = w - pad - btn_w;
        let cancel_x = save_x - btn_w - (8.0 * scale) as i32;

        let is_save_hovered = state.hovered_target == ModelsPopupHit::SaveBtn;
        draw_button(
            dib_dc,
            bits,
            w,
            h,
            save_x,
            btn_y,
            btn_w,
            btn_h,
            "Save",
            theme,
            body_font,
            is_save_hovered,
            ButtonStyle::Primary,
        );

        let is_cancel_hovered = state.hovered_target == ModelsPopupHit::CancelBtn;
        draw_button(
            dib_dc,
            bits,
            w,
            h,
            cancel_x,
            btn_y,
            btn_w,
            btn_h,
            "Cancel",
            theme,
            body_font,
            is_cancel_hovered,
            ButtonStyle::Secondary,
        );

        // ── Final render ───────────────────────────────────────────
        let _ = DeleteObject(title_font);
        let _ = DeleteObject(body_font);
        let _ = DeleteObject(small_font);

        frame.fix_gdi_alpha(theme.background);

        let cur_pos = {
            let mut wr = RECT::default();
            let _ = GetWindowRect(hwnd, &mut wr);
            (wr.left, wr.top)
        };
        frame.present_layered(hwnd, cur_pos.0, cur_pos.1, 255);
    }
}

// ── Hit-test ─────────────────────────────────────────────────────────

fn hit_test_models_popup(state: &ModelsPopupState, x: i32, y: i32) -> ModelsPopupHit {
    let scale = state.scale;
    let pad = (POPUP_PADDING as f32 * scale) as i32;
    let header_h = (POPUP_HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let footer_h = (POPUP_FOOTER_HEIGHT_BASE as f32 * scale) as i32;
    let footer_y = state.win_h - footer_h;
    let w = state.win_w;
    let _h = state.win_h;
    let row_h = (POPUP_ROW_HEIGHT as f32 * scale) as i32;
    let field_h = (POPUP_FIELD_HEIGHT as f32 * scale) as i32;
    let del_w = (28.0 * scale) as i32;
    let scrollbar_w = (8.0 * scale) as i32;
    let test_btn_w = (50.0 * scale) as i32;
    let btn_gap = (4.0 * scale) as i32;
    let scroll_y = state.content_scroll_y;

    let list_y = header_h + pad;
    let list_h = state.list_area_h;
    let label_y = list_y;
    let label_h = (16.0 * scale) as i32;
    let list_content_top = label_y + label_h;

    // Select All / Deselect All buttons (label row)
    let sel_btn_w = (50.0 * scale) as i32;
    let sel_btn_h = label_h;
    let sel_btn_y = label_y;
    let deselect_x = w - pad - sel_btn_w;
    let select_x = deselect_x - (4.0 * scale) as i32 - sel_btn_w;
    if y >= sel_btn_y && y < sel_btn_y + sel_btn_h {
        if x >= select_x && x < select_x + sel_btn_w {
            return ModelsPopupHit::SelectAllBtn;
        }
        if x >= deselect_x && x < deselect_x + sel_btn_w {
            return ModelsPopupHit::DeselectAllBtn;
        }
    }
    let fetch_btn_w = (100.0 * scale) as i32;
    let fetch_btn_h = (28.0 * scale) as i32;
    let fetch_btn_x = w - pad - fetch_btn_w;
    let fetch_btn_y = (header_h - fetch_btn_h) / 2;
    if y >= fetch_btn_y
        && y < fetch_btn_y + fetch_btn_h
        && x >= fetch_btn_x
        && x < fetch_btn_x + fetch_btn_w
    {
        return ModelsPopupHit::FetchModelsBtn;
    }

    // Footer buttons
    let btn_h = (30.0 * scale) as i32;
    let btn_w = (90.0 * scale) as i32;
    let btn_y = footer_y + (footer_h - btn_h) / 2;
    let save_x = w - pad - btn_w;
    let cancel_x = save_x - btn_w - (8.0 * scale) as i32;

    if y >= btn_y && y < btn_y + btn_h {
        if x >= save_x && x < save_x + btn_w {
            return ModelsPopupHit::SaveBtn;
        }
        if x >= cancel_x && x < cancel_x + btn_w {
            return ModelsPopupHit::CancelBtn;
        }
    }

    // Scrollbar area
    let scrollbar_x = w - pad - scrollbar_w;
    if x >= scrollbar_x && x < scrollbar_x + scrollbar_w && y >= list_y && y < list_y + list_h {
        return ModelsPopupHit::Scrollbar;
    }

    // Model rows + add button (inside list area, clipped)
    if y >= list_y && y < list_y + list_h {
        let total_rows = state.models.len() as i32;
        let add_btn_y = list_content_top + total_rows * row_h - scroll_y;

        // Check add button
        if y >= add_btn_y && y < add_btn_y + row_h {
            if x >= pad && x < pad + (120.0 * scale) as i32 {
                return ModelsPopupHit::AddBtn;
            }
        }

        // Check each model row
        for i in 0..total_rows {
            let ry = list_content_top + i * row_h - scroll_y;
            if y >= ry && y < ry + row_h {
                // Button positions
                let del_x = w - pad - scrollbar_w - del_w;
                let test_btn_x = del_x - btn_gap - test_btn_w;

                // Check delete X button (rightmost)
                if x >= del_x && x < del_x + del_w {
                    return ModelsPopupHit::ModelDelete(i as usize);
                }
                // Check test button
                if x >= test_btn_x && x < test_btn_x + test_btn_w {
                    return ModelsPopupHit::ModelTestBtn(i as usize);
                }
                // Check checkbox (left side)
                let checkbox_w = (16.0 * scale) as i32;
                let checkbox_h = (16.0 * scale) as i32;
                let chk_x = pad;
                let chk_y = ry + (row_h - checkbox_h) / 2;
                if x >= chk_x && x < chk_x + checkbox_w && y >= chk_y && y < chk_y + checkbox_h {
                    return ModelsPopupHit::ModelCheckbox(i as usize);
                }
                // Check text field
                let field_left = pad + checkbox_w + (6.0 * scale) as i32;
                let field_right = test_btn_x - btn_gap;
                let field_top = ry + (row_h - field_h) / 2;
                let field_bottom = ry + (row_h + field_h) / 2;
                if x >= field_left && x < field_right && y >= field_top && y < field_bottom {
                    return ModelsPopupHit::ModelField(i as usize);
                }
            }
        }
    }

    ModelsPopupHit::None
}

// ── Inline editing helpers ───────────────────────────────────────────

fn commit_model_edit(state: &mut ModelsPopupState) {
    if let Some(idx) = state.editing_idx {
        // Don't remove models on commit, just save the text
        if idx < state.models.len() {
            state.models[idx] = state.edit_text.clone();
        }
    }
    state.editing_idx = None;
    state.edit_text.clear();
    state.edit_cursor = 0;
    state.edit_old_value.clear();
}

fn cancel_model_edit(state: &mut ModelsPopupState) {
    if state.editing_idx.is_none() {
        return;
    }
    // Restore original value
    state.edit_text = state.edit_old_value.clone();
    state.edit_cursor = state.edit_old_value.len();
    commit_model_edit(state);
}

fn begin_model_edit(state: &mut ModelsPopupState, idx: usize) {
    commit_model_edit(state);
    if idx < state.models.len() {
        state.editing_idx = Some(idx);
        state.edit_text = state.models[idx].clone();
        state.edit_cursor = state.edit_text.len();
        state.edit_old_value = state.edit_text.clone();
    }
}

/// Insert text from the clipboard at the cursor position.
fn paste_from_clipboard(text: &mut String, cursor: &mut usize) {
    unsafe {
        if OpenClipboard(None).is_err() {
            return;
        }
        let handle = match GetClipboardData(13u32) {
            Ok(h) => h,
            Err(_) => {
                let _ = CloseClipboard();
                return;
            }
        };
        if handle.is_invalid() {
            let _ = CloseClipboard();
            return;
        }
        let hglobal = windows::Win32::Foundation::HGLOBAL(handle.0);
        let ptr = GlobalLock(hglobal) as *const u16;
        if ptr.is_null() {
            let _ = CloseClipboard();
            return;
        }
        let buf = std::slice::from_raw_parts(ptr, 65536);
        let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
        if len > 0 {
            if let Ok(s) = String::from_utf16(&buf[..len]) {
                text.insert_str(*cursor, &s);
                *cursor += s.len();
            }
        }
        let _ = GlobalUnlock(hglobal);
        let _ = CloseClipboard();
    }
}

// ── Background model fetch ────────────────────────────────────────────

/// Query the provider's `/models` endpoint and return the list of model IDs.
fn fetch_available_models(endpoint: &str, api_key: &str) -> Option<Vec<String>> {
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(_) => return None,
    };

    let base = endpoint.trim_end_matches('/');
    let models_url = format!("{base}/models");

    let mut headers = reqwest::header::HeaderMap::new();
    if !api_key.is_empty() {
        let Ok(hdr) = format!("Bearer {api_key}").parse::<reqwest::header::HeaderValue>() else {
            return None;
        };
        headers.insert(reqwest::header::AUTHORIZATION, hdr);
    }
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("application/json"),
    );

    let resp = match client.get(&models_url).headers(headers).send() {
        Ok(r) if r.status().is_success() => r,
        _ => return None,
    };

    let body: serde_json::Value = match resp.json() {
        Ok(v) => v,
        Err(_) => return None,
    };

    // OpenAI returns { "data": [{ "id": "..." }, ...] }, some providers return a plain array.
    let models_list: &[serde_json::Value] = body
        .get("data")
        .and_then(|d| d.as_array())
        .or_else(|| body.as_array())
        .map_or(&[], |v| v.as_slice());

    let ids: Vec<String> = models_list
        .iter()
        .filter_map(|v| {
            v.get("id")
                .and_then(|id| id.as_str())
                .or_else(|| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();

    if ids.is_empty() { None } else { Some(ids) }
}

// ── Background model test ────────────────────────────────────────────

/// Send a minimal ping to a specific model and return true if successful.
fn run_single_model_test(
    endpoint: &str,
    api_key: &str,
    model_id: &str,
    cancelled: &AtomicBool,
) -> bool {
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    let base = endpoint.trim_end_matches('/');
    let chat_url = format!("{base}/chat/completions");

    let mut headers = reqwest::header::HeaderMap::new();
    if !api_key.is_empty() {
        let Ok(hdr) = format!("Bearer {api_key}").parse::<reqwest::header::HeaderValue>() else {
            return false;
        };
        headers.insert(reqwest::header::AUTHORIZATION, hdr);
    }
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("application/json"),
    );

    if cancelled.load(Ordering::SeqCst) {
        return false;
    }

    let ping_body = serde_json::json!({
        "model": model_id,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 50,
        "stream": false,
    });

    let resp = match client
        .post(&chat_url)
        .headers(headers)
        .json(&ping_body)
        .send()
    {
        Ok(r) => r,
        Err(_) => return false,
    };

    if cancelled.load(Ordering::SeqCst) {
        return false;
    }

    resp.status().is_success()
}

// ── Window procedure ─────────────────────────────────────────────────

unsafe extern "system" fn models_popup_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => LRESULT(0),

            WM_NCHITTEST => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ModelsPopupState;
                if state_ptr.is_null() {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
                let state = &*state_ptr;
                let screen_x = (lparam.0 as i16) as i32;
                let screen_y = ((lparam.0 >> 16) as i16) as i32;
                let mut pt = POINT {
                    x: screen_x,
                    y: screen_y,
                };
                let _ = ScreenToClient(hwnd, &mut pt);

                let header_h = (POPUP_HEADER_HEIGHT_BASE as f32 * state.scale) as i32;
                let pad = (POPUP_PADDING as f32 * state.scale) as i32;
                if pt.y < header_h {
                    // Check if the click is on the Get Models button — if so, treat as client area
                    let get_btn_w = (100.0 * state.scale) as i32;
                    let get_btn_h = (28.0 * state.scale) as i32;
                    let get_btn_x = state.win_w - pad - get_btn_w;
                    let get_btn_y = (header_h - get_btn_h) / 2;
                    if pt.x >= get_btn_x
                        && pt.x < get_btn_x + get_btn_w
                        && pt.y >= get_btn_y
                        && pt.y < get_btn_y + get_btn_h
                    {
                        return LRESULT(HTCLIENT as isize);
                    }
                    return LRESULT(HTCAPTION as isize);
                }
                LRESULT(HTCLIENT as isize)
            }

            WM_PAINT => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ModelsPopupState;
                if !state_ptr.is_null() {
                    paint_models_popup(hwnd, state_ptr);
                }
                LRESULT(0)
            }

            WM_LBUTTONDOWN => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ModelsPopupState;
                if state_ptr.is_null() {
                    return LRESULT(0);
                }
                let state = &mut *state_ptr;
                let x = (lparam.0 as i16) as i32;
                let y = ((lparam.0 >> 16) as i16) as i32;
                let hit = hit_test_models_popup(state, x, y);

                // If editing a field and clicking elsewhere, commit the edit
                if state.editing_idx.is_some()
                    && hit != state.hovered_target
                    && hit != ModelsPopupHit::None
                {
                    commit_model_edit(state);
                }

                // Handle scrollbar dragging
                if hit == ModelsPopupHit::Scrollbar {
                    state.is_dragging_scroll = true;
                    state.scroll_drag_start_y = y;
                    state.scroll_drag_start_offset = state.content_scroll_y;
                    return LRESULT(0);
                }

                match hit {
                    ModelsPopupHit::ModelField(i) => {
                        begin_model_edit(state, i);
                        paint_models_popup(hwnd, state_ptr);
                    }
                    ModelsPopupHit::ModelTestBtn(i) => {
                        if state.testing_idx.is_some() {
                            // Already testing another model
                            return LRESULT(0);
                        }
                        commit_model_edit(state);
                        if i < state.models.len() && !state.models[i].trim().is_empty() {
                            let model_id = state.models[i].trim().to_string();
                            let ep = state.endpoint.clone();
                            let ak = state.api_key.clone();
                            let cancelled = state.cancelled.clone();
                            let test_hwnd = state.hwnd.0 as isize;

                            state.testing_idx = Some(i);
                            // Keep previous test result visible while re-testing
                            // (it will be replaced when the result comes back)
                            paint_models_popup(hwnd, state_ptr);

                            std::thread::spawn(move || {
                                let success =
                                    run_single_model_test(&ep, &ak, &model_id, &cancelled);
                                let result_ptr = Box::into_raw(Box::new(success));
                                let _ = PostMessageW(
                                    HWND(test_hwnd as *mut c_void),
                                    WM_MODEL_TEST_RESULT,
                                    WPARAM(i),
                                    LPARAM(result_ptr as isize),
                                );
                            });
                        }
                    }
                    ModelsPopupHit::ModelDelete(i) => {
                        cancel_model_edit(state); // commit any in-progress edit first
                        if i < state.models.len() {
                            state.models.remove(i);
                            if i < state.model_selected.len() {
                                state.model_selected.remove(i);
                            }
                            if i < state.model_test_results.len() {
                                state.model_test_results.remove(i);
                            }
                            // Adjust editing_idx if needed
                            if state.editing_idx == Some(i) {
                                state.editing_idx = None;
                            } else if let Some(ei) = state.editing_idx {
                                if ei > i {
                                    state.editing_idx = Some(ei - 1);
                                }
                            }
                            paint_models_popup(hwnd, state_ptr);
                        }
                    }
                    ModelsPopupHit::AddBtn => {
                        cancel_model_edit(state);
                        state.models.push(String::new());
                        state.model_selected.push(true);
                        state.model_test_results.push(None);
                        let new_idx = state.models.len() - 1;
                        begin_model_edit(state, new_idx);
                        // Scroll to bottom to show the new item
                        let row_h = (POPUP_ROW_HEIGHT as f32 * state.scale) as i32;
                        let total_rows = state.models.len() as i32;
                        let content_h = total_rows * row_h + row_h;
                        let max_scroll = (content_h - state.list_area_h).max(0);
                        state.content_scroll_y = max_scroll;
                        paint_models_popup(hwnd, state_ptr);
                    }
                    ModelsPopupHit::ModelCheckbox(i) => {
                        if i < state.model_selected.len() {
                            state.model_selected[i] = !state.model_selected[i];
                            paint_models_popup(hwnd, state_ptr);
                        }
                    }
                    ModelsPopupHit::SelectAllBtn => {
                        for s in &mut state.model_selected {
                            *s = true;
                        }
                        paint_models_popup(hwnd, state_ptr);
                    }
                    ModelsPopupHit::DeselectAllBtn => {
                        for s in &mut state.model_selected {
                            *s = false;
                        }
                        paint_models_popup(hwnd, state_ptr);
                    }
                    ModelsPopupHit::FetchModelsBtn => {
                        if state.fetching_models {
                            return LRESULT(0);
                        }
                        commit_model_edit(state);
                        let ep = state.endpoint.clone();
                        let ak = state.api_key.clone();
                        let fetch_hwnd = state.hwnd.0 as isize;

                        state.fetching_models = true;
                        paint_models_popup(hwnd, state_ptr);

                        std::thread::spawn(move || {
                            let result = fetch_available_models(&ep, &ak);
                            let result_ptr = match result {
                                Some(models) => Box::into_raw(Box::new(models)) as *mut c_void,
                                None => std::ptr::null_mut(),
                            };
                            let _ = PostMessageW(
                                HWND(fetch_hwnd as *mut c_void),
                                WM_FETCH_MODELS_RESULT,
                                WPARAM(0),
                                LPARAM(result_ptr as isize),
                            );
                        });
                    }
                    ModelsPopupHit::SaveBtn => {
                        commit_model_edit(state);
                        // Filter: keep only selected, non-empty models
                        let mut i = 0;
                        while i < state.models.len() {
                            if state.models[i].is_empty() || !state.model_selected[i] {
                                state.models.remove(i);
                                if i < state.model_selected.len() {
                                    state.model_selected.remove(i);
                                }
                                if i < state.model_test_results.len() {
                                    state.model_test_results.remove(i);
                                }
                            } else {
                                i += 1;
                            }
                        }
                        state.saved = true;
                        state.should_close = true;
                        paint_models_popup(hwnd, state_ptr);
                    }
                    ModelsPopupHit::CancelBtn => {
                        state.should_close = true;
                    }
                    ModelsPopupHit::None | ModelsPopupHit::Scrollbar => {}
                }
                LRESULT(0)
            }

            WM_MODEL_TEST_RESULT => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ModelsPopupState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    let model_idx = wparam.0;
                    let result_ptr = lparam.0 as *mut bool;
                    if !result_ptr.is_null() {
                        let result = Box::from_raw(result_ptr);
                        if model_idx < state.model_test_results.len() {
                            state.model_test_results[model_idx] = Some(*result);
                        }
                    }
                    state.testing_idx = None;
                    paint_models_popup(hwnd, state_ptr);
                }
                LRESULT(0)
            }

            WM_FETCH_MODELS_RESULT => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ModelsPopupState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    let result_ptr = lparam.0 as *mut Vec<String>;
                    if !result_ptr.is_null() {
                        let fetched = Box::from_raw(result_ptr);
                        state.models = *fetched.clone();
                        state.model_selected = (0..state.models.len()).map(|_| true).collect();
                        state.model_test_results = (0..state.models.len()).map(|_| None).collect();
                        // Reset scroll position
                        state.content_scroll_y = 0;
                    }
                    state.fetching_models = false;
                    paint_models_popup(hwnd, state_ptr);
                }
                LRESULT(0)
            }

            WM_LBUTTONUP => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ModelsPopupState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    state.is_dragging_scroll = false;
                }
                LRESULT(0)
            }

            WM_MOUSEMOVE => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ModelsPopupState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    let x = (lparam.0 as i16) as i32;
                    let y = ((lparam.0 >> 16) as i16) as i32;

                    if state.is_dragging_scroll {
                        let dy = y - state.scroll_drag_start_y;
                        let total_rows = state.models.len() as i32;
                        let row_h = (POPUP_ROW_HEIGHT as f32 * state.scale) as i32;
                        let content_h = total_rows * row_h + row_h;
                        let max_scroll = (content_h - state.list_area_h).max(0);
                        // Scale mouse delta by the track-to-content ratio so
                        // the thumb follows the cursor exactly.
                        let list_h = state.list_area_h;
                        let thumb_h =
                            (list_h as f32 * list_h as f32 / content_h as f32).max(20.0) as i32;
                        let track_px = (list_h - thumb_h).max(1);
                        let scaled_dy = if max_scroll > 0 {
                            (dy as f64 * max_scroll as f64 / track_px as f64) as i32
                        } else {
                            0
                        };
                        let new_scroll =
                            (state.scroll_drag_start_offset + scaled_dy).clamp(0, max_scroll);
                        if new_scroll != state.content_scroll_y {
                            state.content_scroll_y = new_scroll;
                            paint_models_popup(hwnd, state_ptr);
                        }
                        return LRESULT(0);
                    }

                    let hit = hit_test_models_popup(state, x, y);
                    if state.hovered_target != hit {
                        state.hovered_target = hit;
                        paint_models_popup(hwnd, state_ptr);
                    }

                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                }
                LRESULT(0)
            }

            WM_MOUSELEAVE => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ModelsPopupState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    if state.hovered_target != ModelsPopupHit::None {
                        state.hovered_target = ModelsPopupHit::None;
                        paint_models_popup(hwnd, state_ptr);
                    }
                }
                LRESULT(0)
            }

            WM_MOUSEWHEEL => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ModelsPopupState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    let delta = (wparam.0 >> 16) as i16 as i32;
                    let total_rows = state.models.len() as i32;
                    let row_h = (POPUP_ROW_HEIGHT as f32 * state.scale) as i32;
                    let content_h = total_rows * row_h + row_h;
                    if content_h > state.list_area_h {
                        let max_scroll = content_h - state.list_area_h;
                        let scroll_amount = (row_h as i32).max(16);
                        let new_scroll = if delta > 0 {
                            (state.content_scroll_y - scroll_amount).max(0)
                        } else {
                            (state.content_scroll_y + scroll_amount).min(max_scroll)
                        };
                        if new_scroll != state.content_scroll_y {
                            state.content_scroll_y = new_scroll;
                            paint_models_popup(hwnd, state_ptr);
                        }
                    }
                }
                LRESULT(0)
            }

            WM_KEYDOWN => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ModelsPopupState;
                if state_ptr.is_null() {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
                let state = &mut *state_ptr;
                if state.editing_idx.is_some() {
                    let vk = wparam.0 as u32;
                    match vk {
                        0x0D /* VK_RETURN */ => {
                            commit_model_edit(state);
                            paint_models_popup(hwnd, state_ptr);
                            return LRESULT(0);
                        }
                        0x1B /* VK_ESCAPE */ => {
                            cancel_model_edit(state);
                            paint_models_popup(hwnd, state_ptr);
                            return LRESULT(0);
                        }
                        0x08 /* VK_BACK */ => {
                            if state.edit_cursor > 0 {
                                let text = &state.edit_text[..state.edit_cursor];
                                if let Some((i, _)) = text.char_indices().next_back() {
                                    state.edit_text.drain(i..state.edit_cursor);
                                    state.edit_cursor = i;
                                    paint_models_popup(hwnd, state_ptr);
                                }
                            }
                            return LRESULT(0);
                        }
                        0x2E /* VK_DELETE */ => {
                            if state.edit_cursor < state.edit_text.len() {
                                let text = &state.edit_text[state.edit_cursor..];
                                if let Some((i, c)) = text.char_indices().next() {
                                    state.edit_text.drain(state.edit_cursor + i..state.edit_cursor + i + c.len_utf8());
                                    paint_models_popup(hwnd, state_ptr);
                                }
                            }
                            return LRESULT(0);
                        }
                        0x25 /* VK_LEFT */ => {
                            if state.edit_cursor > 0 {
                                let text = &state.edit_text[..state.edit_cursor];
                                state.edit_cursor = text.char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
                                paint_models_popup(hwnd, state_ptr);
                            }
                            return LRESULT(0);
                        }
                        0x27 /* VK_RIGHT */ => {
                            if state.edit_cursor < state.edit_text.len() {
                                let text = &state.edit_text[state.edit_cursor..];
                                if let Some((i, c)) = text.char_indices().next() {
                                    state.edit_cursor += i + c.len_utf8();
                                    paint_models_popup(hwnd, state_ptr);
                                }
                            }
                            return LRESULT(0);
                        }
                        0x23 /* VK_END */ => {
                            state.edit_cursor = state.edit_text.len();
                            paint_models_popup(hwnd, state_ptr);
                            return LRESULT(0);
                        }
                        0x24 /* VK_HOME */ => {
                            state.edit_cursor = 0;
                            paint_models_popup(hwnd, state_ptr);
                            return LRESULT(0);
                        }
                        0x56 /* VK_V */ => {
                            if GetKeyState(VK_CONTROL.0 as i32) < 0 {
                                paste_from_clipboard(&mut state.edit_text, &mut state.edit_cursor);
                                paint_models_popup(hwnd, state_ptr);
                            }
                            return LRESULT(0);
                        }
                        _ => {}
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }

            WM_CHAR => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ModelsPopupState;
                if state_ptr.is_null() {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
                let state = &mut *state_ptr;
                if state.editing_idx.is_some() {
                    let ch = (wparam.0 as u32) as u16;
                    if ch >= 0x20 && ch != 0x7F {
                        if let Some(c) = char::from_u32(ch as u32) {
                            state.edit_text.insert(state.edit_cursor, c);
                            state.edit_cursor += c.len_utf8();
                            paint_models_popup(hwnd, state_ptr);
                            return LRESULT(0);
                        }
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }

            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
