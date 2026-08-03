//! Win32 message dispatch for the CPU power-plan overlay.

use super::*;

// ── Message handler ─────────────────────────────────────────────────

pub(super) fn msg_handler(
    hwnd: HWND,
    msg: &MSG,
    st: &mut PanelState,
    drag: &mut Option<(i32, i32)>,
    mouse_tracked: &mut bool,
    hidden: &mut bool,
    sc: f32,
) -> bool {
    if msg.message == WM_ACTIVATE && msg.wParam.0 as u32 == 0 {
        flush_edit(st);
        revert_if_dirty(st); // closing without Save discards live-preview edits
        *hidden = true;
        clear_focus(st);
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
            _ = ReleaseCapture();
            _ = KillTimer(hwnd, TIMER_MONITOR);
        }
        return false;
    }

    if msg.message == WM_LBUTTONDOWN {
        let x = (msg.lParam.0 as i32) & 0xFFFF;
        let y = ((msg.lParam.0 as i32) >> 16) & 0xFFFF;

        // Header: drag or close
        if y < (HDR_H as f32 * sc) as i32 {
            let cx = (W as f32 * sc) as i32 - (PAD as f32 * sc) as i32 - (20.0 * sc) as i32;
            if x >= cx && x <= cx + (20.0 * sc) as i32 {
                flush_edit(st);
                revert_if_dirty(st); // close without Save discards live-preview edits
                *hidden = true;
                clear_focus(st);
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    _ = ReleaseCapture();
                    _ = KillTimer(hwnd, TIMER_MONITOR);
                }
                return false;
            }
            *drag = Some((x, y));
            unsafe {
                let _ = SetCapture(hwnd);
            }
            return true;
        }

        // Plan row: click cycles to next power plan
        if hit_plan_row(y, sc) {
            flush_edit(st);
            // Unsaved edits to the current plan are reverted before leaving it.
            revert_if_dirty(st);
            let cur = st
                .schemes
                .iter()
                .position(|(_, n)| n == &st.active_plan_name)
                .unwrap_or(0);
            let next = (cur + 1) % st.schemes.len().max(1);
            if let Some((guid, name)) = st.schemes.get(next) {
                set_active_scheme(*guid);
                st.active_plan_name = name.clone();
                let (ac_new, dc_new) = read_current_plan_values();
                st.ac = ac_new;
                st.dc = dc_new;
                // New plan becomes the baseline; nothing to save yet.
                commit_baseline(st);
                clear_focus(st);
            }
            return true;
        }

        // Settings rows: value cells and toggles
        let _y_u = (y as f32 / sc) as i32; // unscaled Y

        // Try to hit a value cell
        if let Some(field) = hit_value_cell(x, y, sc) {
            flush_edit(st);
            focus_field(st, field);
            // Give the popup keyboard focus so WM_KEYDOWN/WM_CHAR arrive.
            unsafe {
                let _ = SetForegroundWindow(hwnd);
            }
            return true;
        }

        // Try to hit a toggle
        if hit_toggle(x, y, sc, &mut st.ac, &mut st.dc, &mut st.dirty) {
            flush_edit(st);
            apply_now(st);
            return true;
        }

        // Try to hit a dropdown
        if hit_dropdown(x, y, sc, &mut st.ac, &mut st.dc, &mut st.dirty) {
            flush_edit(st);
            apply_now(st);
            return true;
        }

        // Stress buttons
        if hit_stress_button(x, y, sc, &mut st.monitor, &mut st.stress_handles) {
            flush_edit(st);
            clear_focus(st);
            return true;
        }

        // Save button — commit the live-previewed edits as the new baseline so
        // they are kept. The panel stays open.
        if hit_apply(x, y, sc, st) {
            flush_edit(st);
            apply_now(st);
            commit_baseline(st);
            clear_focus(st);
            return true;
        }

        // Monitor group header → collapse/expand that core group.
        if let Some(g) = hit_core_group_header(y, sc, st) {
            flush_edit(st);
            clear_focus(st);
            match g {
                'p' => st.p_collapsed = !st.p_collapsed,
                _ => st.e_collapsed = !st.e_collapsed,
            }
            return true;
        }

        // Click outside editable fields → commit current edit (applied live)
        if st.focused.is_some() {
            flush_edit(st);
            clear_focus(st);
            unsafe {
                let _ = ReleaseCapture();
            }
        }

        return true;
    }

    if msg.message == WM_LBUTTONUP {
        if drag.is_some() {
            *drag = None;
            unsafe {
                let _ = ReleaseCapture();
            }
        }
        return true;
    }

    if msg.message == WM_MOUSEMOVE {
        let mx = (msg.lParam.0 as i32) & 0xFFFF;
        let my = ((msg.lParam.0 as i32) >> 16) & 0xFFFF;

        if let Some((grab_x, grab_y)) = *drag {
            let mut cursor = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut cursor);
            }
            let nx = cursor.x - grab_x;
            let ny = cursor.y - grab_y;
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    HWND::default(),
                    nx,
                    ny,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER,
                );
            }
            st.pos = POINT { x: nx, y: ny };
        }
        if !*hidden && !*mouse_tracked {
            let mut tm = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                ..Default::default()
            };
            unsafe {
                let _ = TrackMouseEvent(&mut tm);
            }
            *mouse_tracked = true;
        }
        // Update hover state. Do NOT call InvalidateRect here: paint_panel
        // renders via UpdateLayeredWindow and never validates the update region,
        // so a posted WM_PAINT would stay pending forever and trap the PeekMessageW
        // drain loop in a busy spin. The message loop already repaints on every
        // message from this hwnd, so hover changes are reflected automatically.
        let new_hover = hit_hover_row(mx, my, sc);
        if new_hover != st.hover_row {
            st.hover_row = new_hover;
            // Restart the dwell timer whenever the hovered row changes, so the
            // tooltip only appears after the cursor rests on one row.
            st.hover_since = new_hover.map(|_| std::time::Instant::now());
        }
        st.hover_pos = POINT { x: mx, y: my };
        return true;
    }

    if msg.message == WM_MOUSELEAVE {
        *mouse_tracked = false;
        st.hover_row = None;
        st.hover_since = None;
        return true;
    }

    true
}
