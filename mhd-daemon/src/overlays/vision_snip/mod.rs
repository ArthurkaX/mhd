//! Vision Snip overlay — crop, annotate, and analyse a frozen screenshot.
//!
//! Provides [`show`] which opens a full-screen layered window on top of a
//! captured monitor image.  The user can crop, add annotations (point
//! markers, arrows, rectangles), edit descriptions, and send the result
//! for analysis.

pub mod draw;
pub mod editor;
pub mod input;
pub mod model;
pub mod output;
pub mod paint;

use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, ReleaseCapture, SetCapture, SetFocus, VK_CONTROL, VK_ESCAPE, VK_RETURN,
    VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use crate::app::SendHwnd;
use crate::native_theme::NativeTheme;
use crate::osd::to_utf16_z;
use crate::overlays::vision_snip::editor::DescriptionEditor;
use crate::overlays::vision_snip::input::hit_test;
use crate::overlays::vision_snip::model::{
    AnnotationColor, AnnotationGeometry, DragState, Point, Rect, Tool, VisionSnipModel,
};
use crate::overlays::vision_snip::paint::ToolbarAction;
use crate::renderer::DibFrame;
use crate::win32::screen_capture::CapturedImage;

// ── Window class ───────────────────────────────────────────────────────

const CLS: &str = "mhd_vision_snip_cls";

/// Custom message posted from the editor to the main window when done.
const WM_APP_EDITOR_DONE: u32 = WM_APP + 10;

// ── Thread control ─────────────────────────────────────────────────────

static CTRL: Mutex<Option<SendHwnd>> = Mutex::new(None);

// ── Result type ────────────────────────────────────────────────────────

/// The outcome of a Vision Snip session.
#[derive(Debug)]
pub enum VisionSnipResult {
    /// User clicked Analyse.
    Analyze {
        /// The cropped image with annotations rendered onto it.
        image: CapturedImage,
        /// The generated prompt text (embeds the structured annotation
        /// metadata: labels, normalized coordinates, and user descriptions).
        prompt: String,
    },
    /// User cancelled the operation.
    Cancelled,
    /// An error occurred.
    Failed(String),
}

// ── Public API ─────────────────────────────────────────────────────────

/// Open the Vision Snip overlay on the given monitor.
pub fn show(
    theme: NativeTheme,
    hmon_rect: RECT,
    screenshot: CapturedImage,
) -> Receiver<VisionSnipResult> {
    let (tx, rx) = channel();

    std::thread::Builder::new()
        .name("vision-snip".into())
        .spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(theme, hmon_rect, screenshot, tx)
            }));
            if let Err(e) = r {
                let msg = if let Some(s) = e.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = e.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "vision snip panic".to_string()
                };
                eprintln!("mhd: vision_snip thread panicked: {msg}");
            }
        })
        .ok();

    rx
}

// ── Window state ───────────────────────────────────────────────────────

struct SnipState {
    model: VisionSnipModel,
    screenshot: CapturedImage,
    frame: DibFrame,
    scale: f32,
    theme: NativeTheme,
    result_tx: Option<Sender<VisionSnipResult>>,
    mouse_pos: Point,
    hovered_btn: Option<usize>,
    /// Open annotation description editor (None when no editor is active).
    editor: Option<DescriptionEditor>,
    /// Label of the annotation being edited (set alongside editor).
    edit_label: Option<char>,
}

// ── Thread main ────────────────────────────────────────────────────────

fn run(
    theme: NativeTheme,
    hmon_rect: RECT,
    screenshot: CapturedImage,
    result_tx: Sender<VisionSnipResult>,
) {
    let w = hmon_rect.right - hmon_rect.left;
    let h = hmon_rect.bottom - hmon_rect.top;

    if w <= 0 || h <= 0 {
        let _ = result_tx.send(VisionSnipResult::Failed("Invalid monitor rect".into()));
        return;
    }

    // ── Register window class ────────────────────────────────────────
    let cls = to_utf16_z(CLS);
    let hinst: HINSTANCE = match unsafe { GetModuleHandleW(None) } {
        Ok(m) => m.into(),
        Err(_) => return,
    };

    unsafe {
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hCursor: match LoadCursorW(None, IDC_ARROW) {
                Ok(c) => c,
                Err(_) => HCURSOR::default(),
            },
            lpszClassName: PCWSTR::from_raw(cls.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);
    }

    // ── Create layered window ────────────────────────────────────────
    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR::from_raw(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            hmon_rect.left,
            hmon_rect.top,
            w,
            h,
            None,
            None,
            hinst,
            None,
        )
    } {
        Ok(hw) => hw,
        Err(_) => {
            let _ = result_tx.send(VisionSnipResult::Failed("CreateWindowEx failed".into()));
            return;
        }
    };

    // ── Allocate DIB ─────────────────────────────────────────────────
    let frame = match DibFrame::new(w, h) {
        Some(f) => f,
        None => {
            let _ = result_tx.send(VisionSnipResult::Failed(
                "DibFrame allocation failed".into(),
            ));
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return;
        }
    };

    let scale = unsafe { GetDpiForWindow(hwnd) as f32 / 96.0 };

    // ── Model ────────────────────────────────────────────────────────
    let model = VisionSnipModel::new(screenshot.width, screenshot.height);

    // ── State (stack-allocated, pointed to by GWLP_USERDATA) ─────────
    let mut st = SnipState {
        model,
        screenshot,
        frame,
        scale,
        theme,
        result_tx: Some(result_tx),
        mouse_pos: Point { x: 0, y: 0 },
        hovered_btn: None,
        editor: None,
        edit_label: None,
    };

    let state_ptr: *mut SnipState = &mut st;
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
    }

    // ── Store HWND in CTRL ───────────────────────────────────────────
    if let Ok(mut g) = CTRL.lock() {
        *g = Some(SendHwnd(hwnd));
    }

    // ── Initial paint ────────────────────────────────────────────────
    do_paint(&mut st, hwnd);

    // ── Show window ──────────────────────────────────────────────────
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(hwnd);
    }

    // ── Message loop (GetMessageW) ───────────────────────────────────
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0).as_bool() } {
        unsafe {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }

    // ── Cleanup ──────────────────────────────────────────────────────
    if let Some(tx) = st.result_tx.take() {
        let _ = tx.send(VisionSnipResult::Cancelled);
    }
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    if let Ok(mut g) = CTRL.lock() {
        *g = None;
    }
}

// ── WndProc ────────────────────────────────────────────────────────────

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match std::panic::catch_unwind(|| wndproc_inner(hwnd, msg, wp, lp)) {
        Ok(r) => r,
        Err(_) => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// # Safety
///
/// Returns a `&'static mut` reference from a raw pointer stored in
/// `GWLP_USERDATA`.  Callers must ensure no aliasing violations: do not
/// hold the returned reference across `DispatchMessageW` or any other
/// call that may re-enter the window procedure.
unsafe fn state_unchecked(hwnd: HWND) -> Option<&'static mut SnipState> {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if ptr == 0 {
            None
        } else {
            Some(&mut *(ptr as *mut SnipState))
        }
    }
}

fn wndproc_inner(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ACTIVATE => {
            if (wp.0 & 0xFFFF) == 0 {
                // WA_INACTIVE — only cancel if no editor is open
                let has_editor =
                    unsafe { state_unchecked(hwnd) }.map_or(false, |s| s.editor.is_some());
                if !has_editor {
                    if let Some(st) = unsafe { state_unchecked(hwnd) } {
                        send_cancel(st);
                    }
                    unsafe {
                        let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                }
            }
            LRESULT(0)
        }

        WM_KEYDOWN => {
            let vk = wp.0 as u16;
            // Editor-open check is inside handler_keydown
            if let Some(st) = unsafe { state_unchecked(hwnd) } {
                handler_keydown(st, hwnd, vk);
            }
            LRESULT(0)
        }

        WM_LBUTTONDOWN => {
            let (x, y) = unpack_lp(lp);
            if let Some(st) = unsafe { state_unchecked(hwnd) } {
                handler_lbuttondown(st, hwnd, x, y);
            }
            LRESULT(0)
        }

        WM_MOUSEMOVE => {
            let (x, y) = unpack_lp(lp);
            if let Some(st) = unsafe { state_unchecked(hwnd) } {
                handler_mousemove(st, hwnd, x, y);
            }
            LRESULT(0)
        }

        WM_LBUTTONUP => {
            let (x, y) = unpack_lp(lp);
            if let Some(st) = unsafe { state_unchecked(hwnd) } {
                handler_lbuttonup(st, hwnd, x, y);
            }
            LRESULT(0)
        }

        WM_RBUTTONDOWN => {
            let (x, y) = unpack_lp(lp);
            if let Some(st) = unsafe { state_unchecked(hwnd) } {
                handler_rbuttondown(st, hwnd, x, y);
            }
            LRESULT(0)
        }

        WM_SETCURSOR => {
            handler_setcursor(hwnd);
            LRESULT(1)
        }

        WM_PAINT => {
            handler_paint(hwnd);
            LRESULT(0)
        }

        WM_APP_EDITOR_DONE => {
            // Editor has closed — read result and update model.
            unsafe {
                if let Some(st) = state_unchecked(hwnd) {
                    if let Some(editor) = st.editor.take() {
                        let label = st.edit_label.take();
                        if editor.was_applied() {
                            if let Some(lbl) = label {
                                let new_text = editor.get_text().trim().to_string();
                                if !new_text.is_empty() {
                                    let _ = st.model.edit_description(lbl, new_text);
                                }
                            }
                        }
                    }
                    do_paint(st, hwnd);
                }
            }
            LRESULT(0)
        }

        WM_CLOSE => {
            if let Some(st) = unsafe { state_unchecked(hwnd) } {
                send_cancel(st);
            }
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }

        WM_DESTROY => {
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                PostQuitMessage(0);
            }
            LRESULT(0)
        }

        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn handler_paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    unsafe {
        let _ = BeginPaint(hwnd, &mut ps);
        let _ = EndPaint(hwnd, &ps);
    }
    // Layered windows paint via UpdateLayeredWindow, not WM_PAINT,
    // but Begin/EndPaint must still be called to validate the region.
    if let Some(st) = unsafe { state_unchecked(hwnd) } {
        do_paint(st, hwnd);
    }
}

// ── Input handlers ─────────────────────────────────────────────────────

fn handler_keydown(st: &mut SnipState, hwnd: HWND, vk: u16) {
    if st.editor.is_some() {
        return; // editor handles its own keyboard
    }

    if vk == VK_ESCAPE.0 {
        if st.model.pending.is_some() {
            st.model.cancel_pending();
            do_paint(st, hwnd);
        } else {
            send_cancel(st);
            unsafe {
                let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    } else if vk == VK_RETURN.0 {
        let shift = unsafe { (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0 };
        if !shift {
            do_analyze(st, hwnd);
        }
    } else if vk == 0x43 {
        select_tool(st, Tool::Crop, hwnd); // C
    } else if vk == 0x4D {
        select_tool(st, Tool::Marker, hwnd); // M
    } else if vk == 0x41 {
        select_tool(st, Tool::Arrow, hwnd); // A
    } else if vk == 0x52 {
        select_tool(st, Tool::Rectangle, hwnd); // R
    } else if vk == 0x5A {
        // Ctrl+Z
        let ctrl = unsafe { (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0 };
        if ctrl {
            st.model.undo();
            do_paint(st, hwnd);
        }
    }
}

fn select_tool(st: &mut SnipState, tool: Tool, hwnd: HWND) {
    st.model.active_tool = tool;
    st.model.drag = None;
    do_paint(st, hwnd);
}

fn handler_lbuttondown(st: &mut SnipState, hwnd: HWND, x: i32, y: i32) {
    // First check toolbar
    if let Some(action) = hit_test(&st.model, x, y, st.scale) {
        handler_toolbar_action(st, hwnd, action);
        return;
    }

    // Annotation tools clamp to the crop rectangle; the Crop tool itself
    // clamps to the full monitor (it is what defines the crop).
    let pt = clamp_to_active_bounds(&st.model, x, y);
    let (cx, cy) = (pt.x, pt.y);

    match st.model.active_tool {
        Tool::Crop => {
            st.model.drag = Some(DragState::Crop(pt));
            st.model.pending = None;
            unsafe {
                let _ = SetCapture(hwnd);
            }
        }
        Tool::Marker => {
            let badge = clamp_to_active_bounds(&st.model, cx + 20, cy - 24);
            let geom = AnnotationGeometry::Point {
                anchor: pt,
                badge_origin: badge,
            };
            if st.model.start_pending(geom).is_ok() {
                let _ = st.model.commit_pending(String::new());
            }
            do_paint(st, hwnd);
        }
        Tool::Arrow => {
            st.model.drag = Some(DragState::Arrow(pt));
            unsafe {
                let _ = SetCapture(hwnd);
            }
        }
        Tool::Rectangle => {
            st.model.drag = Some(DragState::Rectangle(pt));
            unsafe {
                let _ = SetCapture(hwnd);
            }
        }
        Tool::None => {}
    }
}

fn handler_mousemove(st: &mut SnipState, hwnd: HWND, x: i32, y: i32) {
    // The DragState stores only the START point; the current end is taken
    // from `mouse_pos`. Clamp it to the active drawing bounds (crop for
    // annotations, full monitor for the Crop tool) and do NOT overwrite the
    // drag's start — otherwise the geometry collapses to zero size.
    st.mouse_pos = clamp_to_active_bounds(&st.model, x, y);

    // Hover detection (uses raw coords against the toolbar band).
    let new_hover = hit_test(&st.model, x, y, st.scale).map(|_| compute_hover_index(x, st));
    if new_hover != st.hovered_btn {
        st.hovered_btn = new_hover;
        do_paint(st, hwnd);
    }

    // Drag update — rebuild the pending geometry from start→current.
    if st.model.drag.is_some() {
        update_pending_from_drag(st);
        do_paint(st, hwnd);
    }
}

/// Clamp a raw cursor position to the bounds the active tool may draw in:
/// the full monitor while cropping, the crop rectangle otherwise.
fn clamp_to_active_bounds(model: &VisionSnipModel, x: i32, y: i32) -> Point {
    let (min_x, min_y, max_x, max_y) = if model.active_tool == Tool::Crop {
        (
            0,
            0,
            model.monitor_width as i32 - 1,
            model.monitor_height as i32 - 1,
        )
    } else {
        (
            model.crop.left.max(0),
            model.crop.top.max(0),
            model.crop.right.min(model.monitor_width as i32 - 1),
            model.crop.bottom.min(model.monitor_height as i32 - 1),
        )
    };
    Point {
        x: x.max(min_x).min(max_x),
        y: y.max(min_y).min(max_y),
    }
}

fn handler_lbuttonup(st: &mut SnipState, hwnd: HWND, x: i32, y: i32) {
    let had_drag = st.model.drag.is_some();

    if let Some(drag) = st.model.drag.take() {
        let pt = Point {
            x: x.max(0).min(st.model.monitor_width as i32 - 1),
            y: y.max(0).min(st.model.monitor_height as i32 - 1),
        };

        match drag {
            DragState::Crop(start) => {
                let rect = Rect::new(start.x, start.y, pt.x, pt.y);
                let _ = st.model.set_crop(rect);
            }
            DragState::Arrow(_) | DragState::Rectangle(_) => {
                if st.model.pending.is_some() && st.model.commit_pending(String::new()).is_err() {
                    // Too short / too small — drop the preview instead of leaving it.
                    st.model.cancel_pending();
                }
            }
        }
    }

    if had_drag {
        unsafe {
            let _ = ReleaseCapture();
        }
    }

    do_paint(st, hwnd);
}

fn handler_rbuttondown(st: &mut SnipState, hwnd: HWND, x: i32, y: i32) {
    let click_pt = Point { x, y };
    for ann in &st.model.annotations {
        let badge_pos = draw::badge_center(&ann.geometry);
        let dx = click_pt.x - badge_pos.x;
        let dy = click_pt.y - badge_pos.y;
        let dist2 = (draw::BADGE_R + 4) * (draw::BADGE_R + 4);
        if dx * dx + dy * dy <= dist2 {
            open_editor(st, hwnd, ann.label);
            return;
        }
    }
}

fn handler_setcursor(hwnd: HWND) {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
        let _ = ScreenToClient(hwnd, &mut pt);
    }
    let s = unsafe { GetDpiForWindow(hwnd) as f32 / 96.0 };
    let ty = (paint::TOOL_Y as f32 * s) as i32;
    let th = (paint::TOOL_H as f32 * s) as i32;
    let cursor = if pt.y >= ty && pt.y < ty + th {
        IDC_ARROW
    } else {
        IDC_CROSS
    };
    unsafe {
        let _ = SetCursor(LoadCursorW(None, cursor).unwrap_or_default());
    }
}

// ── Editor management ─────────────────────────────────────────────────

fn open_editor(st: &mut SnipState, hwnd: HWND, label: char) {
    let initial = st
        .model
        .annotations
        .iter()
        .find(|a| a.label == label)
        .map(|a| a.description.clone())
        .unwrap_or_default();

    let editor = match DescriptionEditor::create(hwnd, &st.theme, label, &initial) {
        Some(e) => e,
        None => return,
    };

    st.editor = Some(editor);
    st.edit_label = Some(label);

    // No nested loop — the editor runs through the main GetMessageW
    // loop.  When it closes, WM_APP_EDITOR_DONE is posted to hwnd
    // and handled in wndproc_inner.
}

// ── Toolbar actions ────────────────────────────────────────────────────

fn handler_toolbar_action(st: &mut SnipState, hwnd: HWND, action: ToolbarAction) {
    match action {
        ToolbarAction::CropTool => {
            if st.model.active_tool == Tool::Crop && st.model.crop != st.model.prev_crop {
                st.model.reset_crop();
            } else {
                st.model.active_tool = Tool::Crop;
            }
            st.model.drag = None;
            do_paint(st, hwnd);
        }
        ToolbarAction::MarkerTool => select_tool(st, Tool::Marker, hwnd),
        ToolbarAction::ArrowTool => select_tool(st, Tool::Arrow, hwnd),
        ToolbarAction::RectangleTool => select_tool(st, Tool::Rectangle, hwnd),
        ToolbarAction::SetColor(idx) => {
            if idx < AnnotationColor::ALL.len() {
                st.model.selected_color = AnnotationColor::ALL[idx];
                do_paint(st, hwnd);
            }
        }
        ToolbarAction::Undo => {
            st.model.undo();
            do_paint(st, hwnd);
        }
        ToolbarAction::Clear => {
            st.model.clear_annotations();
            do_paint(st, hwnd);
        }
        ToolbarAction::Analyze => do_analyze(st, hwnd),
        ToolbarAction::Close | ToolbarAction::CropReset => {
            if matches!(action, ToolbarAction::CropReset) {
                st.model.reset_crop();
                do_paint(st, hwnd);
            } else {
                send_cancel(st);
                unsafe {
                    let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
        }
    }
}

// ── Analyze ────────────────────────────────────────────────────────────

fn do_analyze(st: &mut SnipState, hwnd: HWND) {
    // 1. Commit pending
    if st.model.pending.is_some() {
        if st.model.commit_pending(String::new()).is_err() {
            st.model.cancel_pending();
            do_paint(st, hwnd);
        }
    }

    // 2. Validate (non-fatal)
    let _ = st.model.validate_for_analysis();

    // 3. Crop
    let mut cropped = match output::crop_image(&st.screenshot, st.model.crop) {
        Ok(c) => c,
        Err(e) => {
            if let Some(tx) = st.result_tx.take() {
                let _ = tx.send(VisionSnipResult::Failed(format!("Crop failed: {e}")));
            }
            unsafe {
                let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            return;
        }
    };

    // 4. Render crop-local annotations
    let local_anns: Vec<_> = st
        .model
        .annotations
        .iter()
        .map(|ann| {
            let geom = to_crop_local(&ann.geometry, &st.model.crop);
            crate::overlays::vision_snip::model::Annotation {
                label: ann.label,
                geometry: geom,
                color: ann.color,
                description: ann.description.clone(),
            }
        })
        .collect();

    if !local_anns.is_empty() {
        if let Err(e) = output::render_annotations(&mut cropped, &local_anns) {
            eprintln!("mhd: vision_snip render: {e}");
        }
    }

    // 5. Build the structured prompt (embeds annotation metadata).
    let prompt = st.model.build_prompt();

    // 6. Send + close
    if let Some(tx) = st.result_tx.take() {
        let _ = tx.send(VisionSnipResult::Analyze {
            image: cropped,
            prompt,
        });
    }
    unsafe {
        let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

// ── Helpers ────────────────────────────────────────────────────────────

fn send_cancel(st: &mut SnipState) {
    if let Some(tx) = st.result_tx.take() {
        let _ = tx.send(VisionSnipResult::Cancelled);
    }
}

fn do_paint(st: &mut SnipState, hwnd: HWND) {
    let drag_current = if st.model.drag.is_some() {
        Some(st.mouse_pos)
    } else {
        None
    };
    paint::paint_overlay(
        &mut st.frame,
        &st.model,
        &st.screenshot,
        st.scale,
        st.hovered_btn,
        drag_current,
    );
    st.frame.present_layered(hwnd, 0, 0, 255);
}

fn update_pending_from_drag(st: &mut SnipState) {
    let (start, current) = match st.model.drag {
        Some(DragState::Arrow(s)) => (s, st.mouse_pos),
        Some(DragState::Rectangle(s)) => (s, st.mouse_pos),
        _ => return,
    };

    let geom = match st.model.drag {
        Some(DragState::Arrow(_)) => AnnotationGeometry::Arrow {
            from: start,
            to: current,
        },
        Some(DragState::Rectangle(_)) => {
            let rect = Rect::new(start.x, start.y, current.x, current.y);
            AnnotationGeometry::Rectangle { rect }
        }
        _ => return,
    };
    let _ = st.model.start_pending(geom);
}

fn compute_hover_index(x: i32, st: &SnipState) -> usize {
    let sc = |v: i32| (v as f32 * st.scale) as i32;
    let tw = paint::toolbar_width(st.scale);
    let mw = st.model.monitor_width as i32;
    let tx = (mw - tw) / 2;
    let gap = sc(paint::BTN_GAP);
    let sep = sc(paint::SEP_W);
    let bw = sc(paint::BTN_W);
    let sw = sc(paint::SWATCH_SIZE);
    let sw_gap = sc(paint::SWATCH_GAP);

    let mut left = tx + gap;
    let mut idx: usize = 0;

    for _ in 0..4 {
        if x >= left && x < left + bw {
            return idx;
        }
        left += bw + gap;
        idx += 1;
    }
    left += sep;
    for _ in 0..6 {
        if x >= left && x < left + sw {
            return idx;
        }
        left += sw + sw_gap;
        idx += 1;
    }
    left += sep;
    for _ in 0..2 {
        if x >= left && x < left + bw {
            return idx;
        }
        left += bw + gap;
        idx += 1;
    }
    left += sep;
    for _ in 0..2 {
        if x >= left && x < left + bw {
            return idx;
        }
        left += bw + gap;
        idx += 1;
    }
    idx
}

fn unpack_lp(lp: LPARAM) -> (i32, i32) {
    let raw = lp.0 as i32;
    let x = (raw & 0xFFFF) as i16 as i32;
    let y = ((raw >> 16) & 0xFFFF) as i16 as i32;
    (x, y)
}

fn to_crop_local(geom: &AnnotationGeometry, crop: &Rect) -> AnnotationGeometry {
    match geom {
        AnnotationGeometry::Point {
            anchor,
            badge_origin,
        } => AnnotationGeometry::Point {
            anchor: Point {
                x: anchor.x - crop.left,
                y: anchor.y - crop.top,
            },
            badge_origin: Point {
                x: badge_origin.x - crop.left,
                y: badge_origin.y - crop.top,
            },
        },
        AnnotationGeometry::Arrow { from, to } => AnnotationGeometry::Arrow {
            from: Point {
                x: from.x - crop.left,
                y: from.y - crop.top,
            },
            to: Point {
                x: to.x - crop.left,
                y: to.y - crop.top,
            },
        },
        AnnotationGeometry::Rectangle { rect } => AnnotationGeometry::Rectangle {
            rect: Rect {
                left: rect.left - crop.left,
                top: rect.top - crop.top,
                right: rect.right - crop.left,
                bottom: rect.bottom - crop.top,
            },
        },
    }
}
