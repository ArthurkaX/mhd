use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use eframe::egui;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, FindWindowW, ShowWindow, SM_CXSCREEN, SM_CYSCREEN, SW_SHOW,
};
use windows::core::PCWSTR;

#[cfg(windows)]
use winit::platform::windows::EventLoopBuilderExtWindows;

lazy_static::lazy_static! {
    pub static ref UI_STATE: Arc<Mutex<UiState>> = Arc::new(Mutex::new(UiState::default()));
}

#[derive(Default)]
pub struct UiState {
    pub brightness_visible: bool,
    pub brightness_value: u32,
    pub monitor_name: String,
    pub last_update: Option<Instant>,
    pub about_visible: bool,
    pub theme: Option<egui::Visuals>,
    pub should_exit: bool,
    pub ctx: Option<egui::Context>,
}

fn force_wake_window() {
    let title: Vec<u16> = "mhd_overlay\0".encode_utf16().collect();
    unsafe {
        if let Ok(hwnd) = FindWindowW(PCWSTR::null(), PCWSTR::from_raw(title.as_ptr())) {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
    }
}

pub fn show_brightness(value: u32, name: String) {
    println!("mhd: UI: show_brightness({}, {})", value, name);
    let ctx = {
        let mut state = match UI_STATE.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        state.brightness_value = value;
        state.monitor_name = name;
        state.brightness_visible = true;
        state.last_update = Some(Instant::now());
        state.ctx.clone()
    };
    
    if let Some(ctx) = ctx {
        force_wake_window();
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.request_repaint();
    }
}

pub fn show_about() {
    println!("mhd: UI: show_about()");
    let ctx = {
        if let Ok(mut state) = UI_STATE.lock() {
            state.about_visible = true;
            state.ctx.clone()
        } else {
            None
        }
    };

    if let Some(ctx) = ctx {
        force_wake_window();
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.request_repaint();
    }
}

pub fn shutdown() {
    let ctx = {
        if let Ok(mut state) = UI_STATE.lock() {
            state.should_exit = true;
            state.ctx.clone()
        } else {
            None
        }
    };

    if let Some(ctx) = ctx {
        force_wake_window();
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.request_repaint();
    }
}

pub struct OverlayApp {
    is_currently_visible: bool,
}

impl OverlayApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        if let Ok(mut state) = UI_STATE.lock() {
            state.ctx = Some(cc.egui_ctx.clone());
            if let Some(visuals) = &state.theme {
                cc.egui_ctx.set_visuals(visuals.clone());
            }
        }

        Self {
            is_currently_visible: false,
        }
    }
}

impl eframe::App for OverlayApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let (brightness_visible, brightness_value, monitor_name, about_visible, theme, should_exit) = {
            let mut state = match UI_STATE.lock() {
                Ok(s) => s,
                Err(_) => return,
            };

            if state.should_exit {
                (false, 0, String::new(), false, None, true)
            } else {
                if let Some(last) = state.last_update
                    && last.elapsed() > Duration::from_secs(2)
                        && state.brightness_visible {
                            println!("mhd: UI: brightness timeout, hiding");
                            state.brightness_visible = false;
                        }

                (
                    state.brightness_visible,
                    state.brightness_value,
                    state.monitor_name.clone(),
                    state.about_visible,
                    state.theme.clone(),
                    false,
                )
            }
        };

        if should_exit {
            println!("mhd: UI: exiting");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        let should_be_visible = about_visible || brightness_visible;

        if should_be_visible && !self.is_currently_visible {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            self.is_currently_visible = true;
        } else if !should_be_visible && self.is_currently_visible {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.is_currently_visible = false;
        }

        if about_visible {
            show_about_ui(ctx, theme.clone());
        } else if brightness_visible {
            show_brightness_ui(ctx, brightness_value, &monitor_name, theme.clone());
            // Only poll for the timeout when the brightness UI is actually visible
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Color32::TRANSPARENT.to_normalized_gamma_f32()
    }
}

fn show_about_ui(ctx: &egui::Context, theme: Option<egui::Visuals>) {
    let window_size = egui::vec2(320.0, 140.0);
    let screen_width = unsafe { GetSystemMetrics(SM_CXSCREEN) } as f32 / ctx.pixels_per_point();
    let screen_height = unsafe { GetSystemMetrics(SM_CYSCREEN) } as f32 / ctx.pixels_per_point();
    let position = egui::pos2(
        (screen_width - window_size.x) / 2.0,
        (screen_height - window_size.y) / 2.0,
    );

    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(window_size));
    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(position));
    ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(false));

    if let Some(visuals) = &theme {
        ctx.set_visuals(visuals.clone());
    }

    if ctx.input(|i| i.pointer.primary_clicked() || i.viewport().close_requested()) {
        if let Ok(mut state) = UI_STATE.lock() {
            state.about_visible = false;
        }
        return;
    }

    let panel_frame = egui::Frame::window(&ctx.style())
        .fill(ctx.style().visuals.window_fill)
        .stroke(ctx.style().visuals.window_stroke)
        .corner_radius(egui::CornerRadius::same(6))
        .shadow(egui::epaint::Shadow::NONE)
        .inner_margin(egui::Margin::same(16));

    egui::CentralPanel::default().frame(panel_frame).show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(12.0);
            ui.heading(egui::RichText::new("mhd").size(24.0).strong().color(ui.visuals().text_color()));
            ui.add_space(6.0);
            ui.label("Mouse & Hotkey Daemon for Windows");
            ui.add_space(12.0);
            ui.label("Lightweight, single-binary, DDC/CI support.");
        });
    });
}

fn show_brightness_ui(
    ctx: &egui::Context,
    brightness_value: u32,
    monitor_name: &str,
    theme: Option<egui::Visuals>,
) {
    let window_size = egui::vec2(210.0, 50.0);
    let margin = egui::vec2(24.0, 34.0);
    let screen_width = unsafe { GetSystemMetrics(SM_CXSCREEN) } as f32 / ctx.pixels_per_point();
    let screen_height = unsafe { GetSystemMetrics(SM_CYSCREEN) } as f32 / ctx.pixels_per_point();
    let position = egui::pos2(
        screen_width - window_size.x - margin.x,
        screen_height - window_size.y - margin.y,
    );

    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(window_size));
    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(position));
    ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));

    if let Some(visuals) = &theme {
        ctx.set_visuals(visuals.clone());
    }

    let panel_frame = egui::Frame::window(&ctx.style())
        .fill(ctx.style().visuals.window_fill)
        .stroke(ctx.style().visuals.window_stroke)
        .corner_radius(egui::CornerRadius::same(3))
        .shadow(egui::epaint::Shadow::NONE)
        .inner_margin(egui::Margin {
            left: 8,
            right: 8,
            top: 6,
            bottom: 6,
        });

    egui::CentralPanel::default().frame(panel_frame).show(ctx, |ui| {
        // Monitor Name
        ui.label(
            egui::RichText::new(monitor_name)
                .color(ui.visuals().text_color())
                .size(8.0),
        );

        ui.add_space(6.0);

        // Slider Track Row
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            let track_width = 164.0;
            let spacing = 6.0;
            let thumb_height = 14.0;
            let (rect, _response) = ui.allocate_exact_size(
                egui::vec2(track_width, thumb_height),
                egui::Sense::hover(),
            );

            let fraction = brightness_value as f32 / 100.0;
            let active_color = ui.visuals().selection.bg_fill;
            let track_color = ui.visuals().extreme_bg_color;

            // Track line
            let track_height = 1.0;
            let track_rect = egui::Rect::from_min_size(
                egui::pos2(rect.min.x, rect.center().y - track_height / 2.0),
                egui::vec2(rect.width(), track_height),
            );

            ui.painter().rect_filled(track_rect, 0.0, track_color);

            // Filled track
            let filled_width = rect.width() * fraction;
            let filled_rect = egui::Rect::from_min_size(
                track_rect.min,
                egui::vec2(filled_width, track_height),
            );
            ui.painter().rect_filled(filled_rect, 0.0, active_color);

            // Thumb block
            let thumb_width = 4.0;
            let thumb_x = (track_rect.min.x + filled_width - thumb_width / 2.0)
                .clamp(rect.min.x, rect.max.x - thumb_width);

            let thumb_rect = egui::Rect::from_min_size(
                egui::pos2(thumb_x, rect.center().y - thumb_height / 2.0),
                egui::vec2(thumb_width, thumb_height),
            );
            ui.painter().rect_filled(thumb_rect, 0.0, active_color);

            ui.add_space(spacing);
            
            // Value text
            ui.label(
                egui::RichText::new(format!("{}", brightness_value))
                    .color(ui.visuals().text_color())
                    .size(8.0),
            );
        });
    });
}

pub fn run_ui_thread() {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("mhd_overlay")
            .with_decorations(false)
            .with_transparent(true)
            .with_active(false)
            .with_visible(false)
            .with_taskbar(false)
            .with_always_on_top()
            .with_position(egui::pos2(-1000.0, -1000.0))
            .with_inner_size(egui::vec2(1.0, 1.0)),
        event_loop_builder: Some(Box::new(|builder| {
            #[cfg(windows)]
            builder.with_any_thread(true);
        })),
        ..Default::default()
    };

    let res = eframe::run_native(
        "mhd_overlay",
        options,
        Box::new(|cc| Ok(Box::new(OverlayApp::new(cc)))),
    );

    if let Err(e) = res {
        eprintln!("mhd: UI error: {e}");
    }
}
