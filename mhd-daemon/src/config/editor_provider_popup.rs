//! Provider editor modal popup for the LLM Proxy settings.
//!
//! Opens a small modal dialog with two text fields (name, endpoint) and
//! Save / Cancel buttons.  Follows the same pattern as
//! `editor_binding_popup` but is simpler — no key capture, no combo boxes,
//! no recordings.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use crate::config::editor_layout::*;
use crate::config::editor_state::{ButtonStyle, SettingsState, UiProvider};
use crate::config::editor_theme::{
    draw_button, draw_plain_label, draw_rounded_border_in_buffer, draw_rounded_rect_in_buffer,
    to_utf16_z,
};
use crate::core::native_theme::{Argb, NativeTheme};

// ── Constants ────────────────────────────────────────────────────────

const POPUP_WIDTH_BASE: i32 = 460;
const POPUP_HEIGHT_BASE: i32 = 520;
const POPUP_HEADER_HEIGHT_BASE: i32 = 48;
const POPUP_FOOTER_HEIGHT_BASE: i32 = 52;
const POPUP_RADIUS_BASE: f32 = 12.0;
const POPUP_PADDING: i32 = 20;
const POPUP_FIELD_HEIGHT: i32 = 30;
const POPUP_LABEL_WIDTH: i32 = 100;
const POPUP_ROW_HEIGHT: i32 = 32;

/// Custom message posted by the test worker thread back to the popup.
/// `WPARAM` = 0 (in-progress), 1 (success), 2 (failure).
/// `LPARAM` = pointer to a heap-allocated String with the status message.
const WM_PROVIDER_TEST_UPDATE: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 2;

// ── Which field is currently being inline-edited ─────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditField {
    None,
    Name,
    Endpoint,
    ApiKey,
    AnthropicKey,
    BindAddress,
}

// ── Popup state ──────────────────────────────────────────────────────

struct ProviderPopupState {
    hwnd: HWND,
    _parent_hwnd: HWND,
    _parent_ptr: *mut SettingsState,

    // Edited data (cloned from parent on open)
    name: String,
    endpoint: String,
    api_key: String,
    anthropic_key: String,
    bind_address: String,
    models: Vec<String>,
    _original_name: String,
    _original_endpoint: String,
    _original_api_key: String,
    _original_anthropic_key: String,
    _original_bind_address: String,
    _original_models: Vec<String>,

    // Inline editing state
    editing_field: EditField,
    edit_text: String,
    edit_cursor: usize,
    edit_old_value: String,

    // Test state
    test_in_progress: bool,
    test_success: bool,
    test_message: String,
    /// Shared flag that the background thread checks periodically.
    /// Set to true when the popup closes.
    cancelled: Arc<AtomicBool>,

    // UI state
    theme: NativeTheme,
    scale: f32,
    win_w: i32,
    win_h: i32,
    hovered_target: ProviderPopupHit,
    should_close: bool,
    saved: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderPopupHit {
    None,
    NameField,
    EndpointField,
    ApiKeyField,
    AnthropicKeyField,
    BindAddressField,
    TestBtn,
    SaveBtn,
    CancelBtn,
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
        let class_name = to_utf16_z("mhd_ProviderEditorPopup");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(provider_popup_wndproc),
            cbClsExtra: 0,
            cbWndExtra: std::mem::size_of::<isize>() as i32, // GWLP_USERDATA
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

/// Open the provider editor popup as a modal dialog.
/// Returns the edited `UiProvider` if saved, or `None` if cancelled.
pub fn open_provider_popup(
    parent_hwnd: HWND,
    parent_ptr: *mut SettingsState,
    provider: Option<UiProvider>,
) -> Option<UiProvider> {
    let scale = unsafe { &*parent_ptr }.layout.scale();

    let win_w = (POPUP_WIDTH_BASE as f32 * scale) as i32;
    let win_h = (POPUP_HEIGHT_BASE as f32 * scale) as i32;

    // Center on parent window
    let mut parent_rc = RECT::default();
    unsafe {
        let _ = GetWindowRect(parent_hwnd, &mut parent_rc);
    }
    let cx = parent_rc.left + (parent_rc.right - parent_rc.left - win_w) / 2;
    let cy = parent_rc.top + (parent_rc.bottom - parent_rc.top - win_h) / 2;

    let theme = unsafe { (*parent_ptr).theme.clone() };

    let (name, endpoint, api_key, models) = match provider {
        Some(ref p) => (
            p.name.clone(),
            p.endpoint.clone(),
            p.api_key.clone(),
            p.models.clone(),
        ),
        None => (String::new(), String::new(), String::new(), Vec::new()),
    };

    // Read global proxy settings from parent state
    let anthropic_key = unsafe { (*parent_ptr).anthropic_key.clone() };
    let bind_address = unsafe { (*parent_ptr).proxy_bind_address.clone() };

    let popup_state = Box::new(ProviderPopupState {
        hwnd: HWND::default(),
        _parent_hwnd: parent_hwnd,
        _parent_ptr: parent_ptr,
        name: name.clone(),
        endpoint: endpoint.clone(),
        api_key: api_key.clone(),
        anthropic_key: anthropic_key.clone(),
        bind_address: bind_address.clone(),
        models: models.clone(),
        _original_name: name,
        _original_endpoint: endpoint,
        _original_api_key: api_key,
        _original_anthropic_key: anthropic_key,
        _original_bind_address: bind_address,
        _original_models: models,
        editing_field: EditField::None,
        edit_text: String::new(),
        edit_cursor: 0,
        edit_old_value: String::new(),
        test_in_progress: false,
        test_success: false,
        test_message: String::new(),
        cancelled: Arc::new(AtomicBool::new(false)),
        theme,
        scale,
        win_w,
        win_h,
        hovered_target: ProviderPopupHit::None,
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
            PCWSTR::from_raw(to_utf16_z("Edit Provider").as_ptr()),
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
        paint_provider_popup(hwnd, popup_ptr);
    }

    // Modal message loop
    loop {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
        if state_ptr == 0 {
            break;
        }
        let should_close = unsafe { (*(state_ptr as *mut ProviderPopupState)).should_close };
        if should_close {
            break;
        }

        unsafe {
            let _ = WaitMessage();
        }
    }

    // Re-enable parent and bring it to front
    unsafe {
        let _ = EnableWindow(parent_hwnd, true);
        let _ = SetForegroundWindow(parent_hwnd);
    }

    let saved = unsafe { (*popup_ptr).saved };

    // Write back global proxy settings to parent state
    if saved {
        unsafe {
            (*parent_ptr).anthropic_key = (*popup_ptr).anthropic_key.clone();
            (*parent_ptr).proxy_bind_address = (*popup_ptr).bind_address.clone();
        }
    }

    let result = if saved {
        unsafe {
            Some(UiProvider {
                name: (*popup_ptr).name.clone(),
                endpoint: (*popup_ptr).endpoint.clone(),
                api_key: (*popup_ptr).api_key.clone(),
                models: (*popup_ptr).models.clone(),
            })
        }
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

#[allow(clippy::too_many_arguments)]
unsafe fn paint_provider_popup(hwnd: HWND, state_ptr: *mut ProviderPopupState) {
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
        let mut title_wz = to_utf16_z("Edit Provider");
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

        // ── Content layout ─────────────────────────────────────────
        let field_h = (POPUP_FIELD_HEIGHT as f32 * scale) as i32;
        let row_h = (POPUP_ROW_HEIGHT as f32 * scale) as i32;
        let label_w = (POPUP_LABEL_WIDTH as f32 * scale) as i32;
        let content_y = header_h + (12.0 * scale) as i32;
        let field_x = pad + label_w + 8;

        // Helper: draw a text field with inline editing support
        let draw_text_field = |dib_dc: HDC,
                               bits: *mut c_void,
                               w: i32,
                               h: i32,
                               scale: f32,
                               rect: RECT,
                               text: &str,
                               is_hovered: bool,
                               is_editing: bool,
                               cursor_pos: usize| {
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
            draw_rounded_rect_in_buffer(bits, w, h, rect, (4.0 * scale) as i32, bg);
            draw_rounded_border_in_buffer(bits, w, h, rect, (4.0 * scale) as i32, 1, border);

            // Draw text clipped to field interior
            let mut text_rect = RECT {
                left: rect.left + (6.0 * scale) as i32,
                top: rect.top,
                right: rect.right - (6.0 * scale) as i32,
                bottom: rect.bottom,
            };

            SelectObject(dib_dc, small_font);
            SetTextColor(dib_dc, theme.text.to_colorref());
            let mut text_wz = to_utf16_z(text);
            let _ = DrawTextW(
                dib_dc,
                &mut text_wz,
                &mut text_rect,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );

            // Draw cursor if editing
            if is_editing && cursor_pos <= text.len() {
                let prefix = &text[..cursor_pos];
                let prefix_wz: Vec<u16> = prefix.encode_utf16().collect();
                let mut sz = SIZE::default();
                let _ = GetTextExtentPoint32W(dib_dc, &prefix_wz, &mut sz);
                let cursor_x = text_rect.left + sz.cx;
                let cursor_y = rect.top + (rect.bottom - rect.top - (12.0 * scale) as i32) / 2;
                let cursor_height = (12.0 * scale) as i32;
                let cursor_pen = CreatePen(PS_SOLID, 1, theme.text.to_colorref());
                let _ = SelectObject(dib_dc, cursor_pen);
                let _ = MoveToEx(dib_dc, cursor_x, cursor_y, None);
                let _ = LineTo(dib_dc, cursor_x, cursor_y + cursor_height);
                let _ = DeleteObject(cursor_pen);
            }
        };

        // ── Name row ──────────────────────────────────────────────
        let name_label_y = content_y;
        draw_plain_label(
            dib_dc,
            RECT {
                left: pad,
                top: name_label_y,
                right: pad + label_w,
                bottom: name_label_y + row_h,
            },
            "Name",
            body_font,
            theme.text,
        );

        let name_field_rect = RECT {
            left: field_x,
            top: name_label_y,
            right: w - pad,
            bottom: name_label_y + field_h,
        };
        let name_is_hovered = state.hovered_target == ProviderPopupHit::NameField;
        let name_is_editing = state.editing_field == EditField::Name;
        let name_display = if name_is_editing {
            &state.edit_text
        } else {
            &state.name
        };
        draw_text_field(
            dib_dc,
            bits,
            w,
            h,
            scale,
            name_field_rect,
            name_display,
            name_is_hovered,
            name_is_editing,
            state.edit_cursor,
        );

        // ── Endpoint row ───────────────────────────────────────────
        let endpoint_label_y = name_label_y + row_h + (8.0 * scale) as i32;
        draw_plain_label(
            dib_dc,
            RECT {
                left: pad,
                top: endpoint_label_y,
                right: pad + label_w,
                bottom: endpoint_label_y + row_h,
            },
            "Endpoint",
            body_font,
            theme.text,
        );

        let endpoint_field_rect = RECT {
            left: field_x,
            top: endpoint_label_y,
            right: w - pad,
            bottom: endpoint_label_y + field_h,
        };
        let ep_is_hovered = state.hovered_target == ProviderPopupHit::EndpointField;
        let ep_is_editing = state.editing_field == EditField::Endpoint;
        let ep_display = if ep_is_editing {
            &state.edit_text
        } else {
            &state.endpoint
        };
        draw_text_field(
            dib_dc,
            bits,
            w,
            h,
            scale,
            endpoint_field_rect,
            ep_display,
            ep_is_hovered,
            ep_is_editing,
            state.edit_cursor,
        );

        // ── API Key row ──────────────────────────────────────────
        let apikey_label_y = endpoint_label_y + row_h + (8.0 * scale) as i32;
        draw_plain_label(
            dib_dc,
            RECT {
                left: pad,
                top: apikey_label_y,
                right: pad + label_w,
                bottom: apikey_label_y + row_h,
            },
            "API Key",
            body_font,
            theme.text,
        );

        let apikey_field_rect = RECT {
            left: field_x,
            top: apikey_label_y,
            right: w - pad,
            bottom: apikey_label_y + field_h,
        };
        let ak_is_hovered = state.hovered_target == ProviderPopupHit::ApiKeyField;
        let ak_is_editing = state.editing_field == EditField::ApiKey;
        let ak_display: String = if ak_is_editing {
            state.edit_text.clone()
        } else if state.api_key.is_empty() {
            "(not set)".into()
        } else {
            let prefix: String = state.api_key.chars().take(8).collect();
            let masked: String = (0..state.api_key.chars().skip(8).count())
                .map(|_| '\u{2022}')
                .collect();
            prefix + &masked
        };
        draw_text_field(
            dib_dc,
            bits,
            w,
            h,
            scale,
            apikey_field_rect,
            &ak_display,
            ak_is_hovered,
            ak_is_editing,
            if ak_is_editing { state.edit_cursor } else { 0 },
        );

        // ── Anthropic Key row ─────────────────────────────────────
        let anth_label_y = apikey_label_y + row_h + (8.0 * scale) as i32;
        draw_plain_label(
            dib_dc,
            RECT {
                left: pad,
                top: anth_label_y,
                right: pad + label_w,
                bottom: anth_label_y + row_h,
            },
            "Anthropic Key",
            body_font,
            theme.text,
        );

        let anth_field_rect = RECT {
            left: field_x,
            top: anth_label_y,
            right: w - pad,
            bottom: anth_label_y + field_h,
        };
        let anth_is_hovered = state.hovered_target == ProviderPopupHit::AnthropicKeyField;
        let anth_is_editing = state.editing_field == EditField::AnthropicKey;
        let anth_display: String = if anth_is_editing {
            state.edit_text.clone()
        } else if state.anthropic_key.is_empty() {
            "(not set — uses OAuth)".into()
        } else {
            let prefix: String = state.anthropic_key.chars().take(8).collect();
            let masked: String = (0..state.anthropic_key.chars().skip(8).count())
                .map(|_| '\u{2022}')
                .collect();
            prefix + &masked
        };
        draw_text_field(
            dib_dc,
            bits,
            w,
            h,
            scale,
            anth_field_rect,
            &anth_display,
            anth_is_hovered,
            anth_is_editing,
            if anth_is_editing {
                state.edit_cursor
            } else {
                0
            },
        );

        // ── Bind Address row ───────────────────────────────────────
        let bind_label_y = anth_label_y + row_h + (8.0 * scale) as i32;
        draw_plain_label(
            dib_dc,
            RECT {
                left: pad,
                top: bind_label_y,
                right: pad + label_w,
                bottom: bind_label_y + row_h,
            },
            "Bind Address",
            body_font,
            theme.text,
        );

        let bind_field_rect = RECT {
            left: field_x,
            top: bind_label_y,
            right: w - pad,
            bottom: bind_label_y + field_h,
        };
        let bind_is_hovered = state.hovered_target == ProviderPopupHit::BindAddressField;
        let bind_is_editing = state.editing_field == EditField::BindAddress;
        let bind_display = if bind_is_editing {
            &state.edit_text
        } else {
            &state.bind_address
        };
        draw_text_field(
            dib_dc,
            bits,
            w,
            h,
            scale,
            bind_field_rect,
            bind_display,
            bind_is_hovered,
            bind_is_editing,
            if bind_is_editing {
                state.edit_cursor
            } else {
                0
            },
        );

        // ── Test button & status ───────────────────────────────────
        let footer_h = (POPUP_FOOTER_HEIGHT_BASE as f32 * scale) as i32;
        let footer_y = h - footer_h;

        let test_y = bind_label_y + row_h + (12.0 * scale) as i32;
        let test_btn_h = (28.0 * scale) as i32;
        let test_btn_w = (100.0 * scale) as i32;
        let test_btn_x = field_x;

        let is_test_hovered = state.hovered_target == ProviderPopupHit::TestBtn;

        if state.test_in_progress {
            // Dimmed button while test is running
            draw_button(
                dib_dc,
                bits,
                w,
                h,
                test_btn_x,
                test_y,
                test_btn_w,
                test_btn_h,
                "Testing…",
                theme,
                body_font,
                false,
                ButtonStyle::Secondary,
            );
        } else {
            draw_button(
                dib_dc,
                bits,
                w,
                h,
                test_btn_x,
                test_y,
                test_btn_w,
                test_btn_h,
                "Test Connection",
                theme,
                body_font,
                is_test_hovered,
                ButtonStyle::Secondary,
            );
        }

        // Test status message
        if !state.test_message.is_empty() {
            let status_y = test_y + test_btn_h + (6.0 * scale) as i32;
            let status_h = (footer_y - status_y - (4.0 * scale) as i32).max(0);
            if status_h > 0 {
                // Draw a subtle background box for the status text
                let status_rect = RECT {
                    left: field_x,
                    top: status_y,
                    right: w - pad,
                    bottom: status_y + status_h,
                };
                draw_rounded_rect_in_buffer(
                    bits,
                    w,
                    h,
                    status_rect,
                    (4.0 * scale) as i32,
                    theme.surface.blend_over(theme.background),
                );

                let status_color = if state.test_in_progress {
                    theme.text_muted
                } else if state.test_success {
                    Argb::new(255, 80, 220, 120) // green
                } else {
                    Argb::new(255, 255, 100, 100) // red
                };

                SelectObject(dib_dc, small_font);
                SetTextColor(dib_dc, status_color.to_colorref());
                let mut status_text = to_utf16_z(&state.test_message);
                let mut status_text_rc = RECT {
                    left: status_rect.left + (6.0 * scale) as i32,
                    top: status_rect.top + (4.0 * scale) as i32,
                    right: status_rect.right - (6.0 * scale) as i32,
                    bottom: status_rect.bottom - (4.0 * scale) as i32,
                };
                let _ = DrawTextW(
                    dib_dc,
                    &mut status_text,
                    &mut status_text_rc,
                    DT_LEFT | DT_WORDBREAK,
                );
            }
        }
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

        let btn_h = (30.0 * scale) as i32;
        let btn_w = (90.0 * scale) as i32;
        let btn_y = footer_y + (footer_h - btn_h) / 2;
        let save_x = w - pad - btn_w;
        let cancel_x = save_x - btn_w - (8.0 * scale) as i32;

        let is_save_hovered = state.hovered_target == ProviderPopupHit::SaveBtn;
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

        let is_cancel_hovered = state.hovered_target == ProviderPopupHit::CancelBtn;
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

fn hit_test_popup(state: &ProviderPopupState, x: i32, y: i32) -> ProviderPopupHit {
    let scale = state.scale;
    let pad = (POPUP_PADDING as f32 * scale) as i32;
    let field_h = (POPUP_FIELD_HEIGHT as f32 * scale) as i32;
    let row_h = (POPUP_ROW_HEIGHT as f32 * scale) as i32;
    let header_h = (POPUP_HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let w = state.win_w;
    let h = state.win_h;
    let label_w = (POPUP_LABEL_WIDTH as f32 * scale) as i32;
    let field_x = pad + label_w + 8;
    let content_y = header_h + (12.0 * scale) as i32;

    let name_label_y = content_y;
    let endpoint_label_y = name_label_y + row_h + (8.0 * scale) as i32;
    let apikey_label_y = endpoint_label_y + row_h + (8.0 * scale) as i32;
    let anth_label_y = apikey_label_y + row_h + (8.0 * scale) as i32;
    let bind_label_y = anth_label_y + row_h + (8.0 * scale) as i32;

    // Footer buttons
    let footer_h = (POPUP_FOOTER_HEIGHT_BASE as f32 * scale) as i32;
    let footer_y = h - footer_h;
    let btn_w = (90.0 * scale) as i32;
    let btn_h = (30.0 * scale) as i32;
    let btn_y = footer_y + (footer_h - btn_h) / 2;
    let save_x = w - pad - btn_w;
    let cancel_x = save_x - btn_w - (8.0 * scale) as i32;

    if y >= btn_y && y < btn_y + btn_h {
        if x >= save_x && x < save_x + btn_w {
            return ProviderPopupHit::SaveBtn;
        }
        if x >= cancel_x && x < cancel_x + btn_w {
            return ProviderPopupHit::CancelBtn;
        }
    }

    // Name field
    let name_field_rect = RECT {
        left: field_x,
        top: name_label_y,
        right: w - pad,
        bottom: name_label_y + field_h,
    };
    if x >= name_field_rect.left
        && x < name_field_rect.right
        && y >= name_field_rect.top
        && y < name_field_rect.bottom
    {
        return ProviderPopupHit::NameField;
    }

    // Endpoint field
    let endpoint_field_rect = RECT {
        left: field_x,
        top: endpoint_label_y,
        right: w - pad,
        bottom: endpoint_label_y + field_h,
    };
    if x >= endpoint_field_rect.left
        && x < endpoint_field_rect.right
        && y >= endpoint_field_rect.top
        && y < endpoint_field_rect.bottom
    {
        return ProviderPopupHit::EndpointField;
    }

    // API Key field
    let apikey_field_rect = RECT {
        left: field_x,
        top: apikey_label_y,
        right: w - pad,
        bottom: apikey_label_y + field_h,
    };
    if x >= apikey_field_rect.left
        && x < apikey_field_rect.right
        && y >= apikey_field_rect.top
        && y < apikey_field_rect.bottom
    {
        return ProviderPopupHit::ApiKeyField;
    }

    // Anthropic Key field
    let anth_field_rect = RECT {
        left: field_x,
        top: anth_label_y,
        right: w - pad,
        bottom: anth_label_y + field_h,
    };
    if x >= anth_field_rect.left
        && x < anth_field_rect.right
        && y >= anth_field_rect.top
        && y < anth_field_rect.bottom
    {
        return ProviderPopupHit::AnthropicKeyField;
    }

    // Bind Address field
    let bind_field_rect = RECT {
        left: field_x,
        top: bind_label_y,
        right: w - pad,
        bottom: bind_label_y + field_h,
    };
    if x >= bind_field_rect.left
        && x < bind_field_rect.right
        && y >= bind_field_rect.top
        && y < bind_field_rect.bottom
    {
        return ProviderPopupHit::BindAddressField;
    }

    // Test button
    let test_y = bind_label_y + row_h + (12.0 * scale) as i32;
    let test_btn_h = (28.0 * scale) as i32;
    let test_btn_w = (100.0 * scale) as i32;
    let test_btn_x = field_x;
    if !state.test_in_progress
        && y >= test_y
        && y < test_y + test_btn_h
        && x >= test_btn_x
        && x < test_btn_x + test_btn_w
    {
        return ProviderPopupHit::TestBtn;
    }

    ProviderPopupHit::None
}

// ── Inline editing helpers ───────────────────────────────────────────

fn begin_edit_field(state: &mut ProviderPopupState, field: EditField) {
    commit_edit_field(state);

    state.editing_field = field;
    state.edit_text = match field {
        EditField::Name => state.name.clone(),
        EditField::Endpoint => state.endpoint.clone(),
        EditField::ApiKey => state.api_key.clone(),
        EditField::AnthropicKey => state.anthropic_key.clone(),
        EditField::BindAddress => state.bind_address.clone(),
        EditField::None => return,
    };
    state.edit_cursor = state.edit_text.len();
    state.edit_old_value = state.edit_text.clone();
}

fn commit_edit_field(state: &mut ProviderPopupState) {
    if state.editing_field == EditField::None {
        return;
    }
    let val = state.edit_text.clone();
    match state.editing_field {
        EditField::Name => state.name = val,
        EditField::Endpoint => state.endpoint = val,
        EditField::ApiKey => state.api_key = val,
        EditField::AnthropicKey => state.anthropic_key = val,
        EditField::BindAddress => state.bind_address = val,
        EditField::None => {}
    }
    state.editing_field = EditField::None;
    state.edit_text.clear();
    state.edit_cursor = 0;
    state.edit_old_value.clear();
}

fn cancel_edit_field(state: &mut ProviderPopupState) {
    if state.editing_field == EditField::None {
        return;
    }
    state.edit_text = state.edit_old_value.clone();
    state.edit_cursor = state.edit_old_value.len();
    commit_edit_field(state);
}

// ── Background provider test ─────────────────────────────────────────

/// Post an update from the background thread to the popup window.
/// `kind`: 0 = in-progress, 1 = success (final), 2 = failure (final).
fn post_test_update(hwnd: HWND, kind: u32, message: String) {
    let msg_ptr = Box::into_raw(Box::new(message));
    let _ = unsafe {
        PostMessageW(
            hwnd,
            WM_PROVIDER_TEST_UPDATE,
            WPARAM(kind as usize),
            LPARAM(msg_ptr as isize),
        )
    };
}

/// Run on a background thread to test a provider endpoint.
/// Posts `WM_PROVIDER_TEST_UPDATE` messages back to `hwnd` for each step.
fn run_provider_test(endpoint: &str, api_key: &str, cancelled: &AtomicBool, hwnd: HWND) {
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            post_test_update(hwnd, 2, format!("Failed to create HTTP client: {e}"));
            return;
        }
    };

    let base = endpoint.trim_end_matches('/');
    let models_url = format!("{base}/models");

    let mut headers = reqwest::header::HeaderMap::new();
    if !api_key.is_empty() {
        let Ok(hdr) = format!("Bearer {api_key}").parse::<reqwest::header::HeaderValue>() else {
            post_test_update(hwnd, 2, "Invalid API key format".into());
            return;
        };
        headers.insert(reqwest::header::AUTHORIZATION, hdr);
    }
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("application/json"),
    );

    if cancelled.load(Ordering::SeqCst) {
        post_test_update(hwnd, 2, "Test cancelled".into());
        return;
    }

    // ── Step 1: Fetch models ────────────────────────────────────────
    post_test_update(hwnd, 0, "Step 1/3: Fetching available models…".into());

    let resp = match client.get(&models_url).headers(headers.clone()).send() {
        Ok(r) => r,
        Err(e) => {
            post_test_update(hwnd, 2, format!("Step 1/3: Connection failed — {e}"));
            return;
        }
    };

    if !resp.status().is_success() {
        post_test_update(
            hwnd,
            2,
            format!("Step 1/3: Models endpoint returned HTTP {}", resp.status()),
        );
        return;
    }

    let body: serde_json::Value = match resp.json() {
        Ok(v) => v,
        Err(e) => {
            post_test_update(hwnd, 2, format!("Step 1/3: Invalid JSON — {e}"));
            return;
        }
    };

    // Extract model list — OpenAI returns { "data": [...] }, some return a plain array.
    let models_list: &[serde_json::Value] = body
        .get("data")
        .and_then(|d| d.as_array())
        .or_else(|| body.as_array())
        .map_or(&[], |v| v.as_slice());

    let model_count = models_list.len();
    if model_count == 0 {
        post_test_update(
            hwnd,
            1,
            "Connected! No models found (check the endpoint URL).".into(),
        );
        return;
    }

    if cancelled.load(Ordering::SeqCst) {
        post_test_update(hwnd, 2, "Test cancelled".into());
        return;
    }

    // ── Step 2: Pick a model (flash preferred) ──────────────────────
    let chosen = models_list
        .iter()
        .find(|m| {
            m.get("id")
                .and_then(|id| id.as_str())
                .map(|s| s.to_lowercase().contains("flash"))
                .unwrap_or(false)
        })
        .or_else(|| models_list.first());

    let model_id = match chosen.and_then(|m| m.get("id").and_then(|id| id.as_str())) {
        Some(id) => id,
        None => {
            post_test_update(
                hwnd,
                1,
                format!("Connected! {model_count} models found, but none have an 'id' field."),
            );
            return;
        }
    };

    post_test_update(
        hwnd,
        0,
        format!("Step 2/3: {model_count} models found — using {model_id}…"),
    );

    if cancelled.load(Ordering::SeqCst) {
        post_test_update(hwnd, 2, "Test cancelled".into());
        return;
    }

    // ── Step 3: Send ping ───────────────────────────────────────────
    post_test_update(hwnd, 0, format!("Step 3/3: Sending ping to {model_id}…"));

    let chat_url = format!("{base}/chat/completions");
    let ping_body = serde_json::json!({
        "model": model_id,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 5,
        "stream": false,
    });

    let start = std::time::Instant::now();
    let ping_resp = match client
        .post(&chat_url)
        .headers(headers)
        .json(&ping_body)
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            post_test_update(
                hwnd,
                2,
                format!("Models OK ({model_count}, using {model_id}) but ping failed: {e}"),
            );
            return;
        }
    };

    if cancelled.load(Ordering::SeqCst) {
        post_test_update(hwnd, 2, "Test cancelled".into());
        return;
    }

    let elapsed = start.elapsed();
    let ms = elapsed.as_millis();

    if !ping_resp.status().is_success() {
        post_test_update(
            hwnd,
            2,
            format!(
                "Models OK ({model_count}, using {model_id}) but ping returned HTTP {}",
                ping_resp.status()
            ),
        );
        return;
    }

    let ping_body: serde_json::Value = match ping_resp.json() {
        Ok(v) => v,
        Err(e) => {
            post_test_update(
                hwnd,
                1,
                format!(
                    "Connected ({model_count}, using {model_id}) but ping response unparseable: {e}"
                ),
            );
            return;
        }
    };

    let content = ping_body
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .unwrap_or("(no content)");

    let has_pong = content.to_lowercase().contains("pong");

    if has_pong {
        post_test_update(
            hwnd,
            1,
            format!(
                "✓ All done!\n\
                 • {model_count} models available\n\
                 • Using model: {model_id}\n\
                 • Ping → Pong in {ms}ms"
            ),
        );
    } else {
        post_test_update(
            hwnd,
            1,
            format!(
                "✓ Connected (no pong)\n\
                 • {model_count} models available\n\
                 • Using model: {model_id}\n\
                 • Response in {ms}ms: {content}"
            ),
        );
    }
}

// ── Window procedure ─────────────────────────────────────────────────

unsafe extern "system" fn provider_popup_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => LRESULT(0),

            WM_NCHITTEST => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ProviderPopupState;
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
                if pt.y < header_h {
                    return LRESULT(HTCAPTION as isize);
                }
                LRESULT(HTCLIENT as isize)
            }

            WM_PAINT => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ProviderPopupState;
                if !state_ptr.is_null() {
                    paint_provider_popup(hwnd, state_ptr);
                }
                LRESULT(0)
            }

            WM_LBUTTONDOWN => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ProviderPopupState;
                if state_ptr.is_null() {
                    return LRESULT(0);
                }
                let state = &mut *state_ptr;
                let x = (lparam.0 as i16) as i32;
                let y = ((lparam.0 >> 16) as i16) as i32;
                let hit = hit_test_popup(state, x, y);

                // If editing a field and clicking elsewhere, commit the edit
                if state.editing_field != EditField::None
                    && hit != state.hovered_target
                    && hit != ProviderPopupHit::None
                {
                    commit_edit_field(state);
                }

                match hit {
                    ProviderPopupHit::TestBtn => {
                        // Commit any in-flight edit first
                        commit_edit_field(state);

                        // Don't start a new test if one is already running
                        if !state.test_in_progress {
                            let ep = state.endpoint.clone();
                            let ak = state.api_key.clone();
                            let cancelled = state.cancelled.clone();
                            // HWND is !Send; pass the raw pointer as isize
                            let test_hwnd = state.hwnd.0 as isize;

                            state.test_in_progress = true;
                            state.test_success = false;
                            state.test_message = String::new();
                            paint_provider_popup(hwnd, state_ptr);

                            // Spawn a background thread to run the test
                            std::thread::spawn(move || {
                                run_provider_test(
                                    &ep,
                                    &ak,
                                    &cancelled,
                                    HWND(test_hwnd as *mut c_void),
                                );
                            });
                        }
                    }
                    ProviderPopupHit::NameField => {
                        begin_edit_field(state, EditField::Name);
                        paint_provider_popup(hwnd, state_ptr);
                    }
                    ProviderPopupHit::EndpointField => {
                        begin_edit_field(state, EditField::Endpoint);
                        paint_provider_popup(hwnd, state_ptr);
                    }
                    ProviderPopupHit::ApiKeyField => {
                        begin_edit_field(state, EditField::ApiKey);
                        paint_provider_popup(hwnd, state_ptr);
                    }
                    ProviderPopupHit::AnthropicKeyField => {
                        begin_edit_field(state, EditField::AnthropicKey);
                        paint_provider_popup(hwnd, state_ptr);
                    }
                    ProviderPopupHit::BindAddressField => {
                        begin_edit_field(state, EditField::BindAddress);
                        paint_provider_popup(hwnd, state_ptr);
                    }
                    ProviderPopupHit::SaveBtn => {
                        commit_edit_field(state);
                        if !state.name.trim().is_empty() {
                            state.saved = true;
                            state.should_close = true;
                        }
                        paint_provider_popup(hwnd, state_ptr);
                    }
                    ProviderPopupHit::CancelBtn => {
                        state.should_close = true;
                    }
                    ProviderPopupHit::None => {}
                }
                LRESULT(0)
            }

            WM_PROVIDER_TEST_UPDATE => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ProviderPopupState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    // Recover the heap-allocated message string
                    let msg_ptr = lparam.0 as *mut String;
                    if !msg_ptr.is_null() {
                        let msg = Box::from_raw(msg_ptr);
                        state.test_message = *msg;
                    }
                    let kind = wparam.0;
                    if kind == 0 {
                        // In-progress: keep test_in_progress true, preserve old test_success
                    } else {
                        state.test_success = kind == 1;
                        state.test_in_progress = false;
                    }
                    paint_provider_popup(hwnd, state_ptr);
                }
                LRESULT(0)
            }

            WM_KEYDOWN => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ProviderPopupState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    if state.editing_field != EditField::None {
                        let vk = wparam.0 as u32;
                        match vk {
                            0x0D /* VK_RETURN */ => {
                                commit_edit_field(state);
                                paint_provider_popup(hwnd, state_ptr);
                            }
                            0x1B /* VK_ESCAPE */ => {
                                cancel_edit_field(state);
                                paint_provider_popup(hwnd, state_ptr);
                            }
                            0x08 /* VK_BACK */ => {
                                if state.edit_cursor > 0 {
                                    let idx = state.edit_cursor - 1;
                                    state.edit_text.remove(idx);
                                    state.edit_cursor -= 1;
                                    paint_provider_popup(hwnd, state_ptr);
                                }
                            }
                            0x2E /* VK_DELETE */ => {
                                if state.edit_cursor < state.edit_text.len() {
                                    state.edit_text.remove(state.edit_cursor);
                                    paint_provider_popup(hwnd, state_ptr);
                                }
                            }
                            0x25 /* VK_LEFT */ => {
                                if state.edit_cursor > 0 {
                                    state.edit_cursor -= 1;
                                    paint_provider_popup(hwnd, state_ptr);
                                }
                            }
                            0x27 /* VK_RIGHT */ => {
                                if state.edit_cursor < state.edit_text.len() {
                                    state.edit_cursor += 1;
                                    paint_provider_popup(hwnd, state_ptr);
                                }
                            }
                            0x23 /* VK_END */ => {
                                state.edit_cursor = state.edit_text.len();
                                paint_provider_popup(hwnd, state_ptr);
                            }
                            0x24 /* VK_HOME */ => {
                                state.edit_cursor = 0;
                                paint_provider_popup(hwnd, state_ptr);
                            }
                            _ => {}
                        }
                    }
                }
                LRESULT(0)
            }

            WM_CHAR => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ProviderPopupState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    if state.editing_field != EditField::None {
                        let ch = (wparam.0 as u32) as u16;
                        if ch >= 0x20 && ch != 0x7F {
                            let c = char::from_u32(ch as u32).unwrap_or(' ');
                            state.edit_text.insert(state.edit_cursor, c);
                            state.edit_cursor += 1;
                            paint_provider_popup(hwnd, state_ptr);
                        }
                    }
                }
                LRESULT(0)
            }

            WM_MOUSEMOVE => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ProviderPopupState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    let x = (lparam.0 as i16) as i32;
                    let y = ((lparam.0 >> 16) as i16) as i32;
                    let hit = hit_test_popup(state, x, y);

                    if state.hovered_target != hit {
                        state.hovered_target = hit;
                        paint_provider_popup(hwnd, state_ptr);
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
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ProviderPopupState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    if state.hovered_target != ProviderPopupHit::None {
                        state.hovered_target = ProviderPopupHit::None;
                        paint_provider_popup(hwnd, state_ptr);
                    }
                }
                LRESULT(0)
            }

            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
