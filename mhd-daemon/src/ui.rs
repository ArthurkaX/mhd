use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use eframe::egui;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
};

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

pub fn show_brightness(value: u32, name: String) {
    println!("mhd: UI: show_brightness({}, {})", value, name);
    let mut state = match UI_STATE.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    state.brightness_value = value;
    state.monitor_name = name;
    state.brightness_visible = true;
    state.last_update = Some(Instant::now());
    if let Some(ctx) = &state.ctx {
        ctx.request_repaint();
    }
}

pub fn show_about() {
    println!("mhd: UI: show_about()");
    if let Ok(mut state) = UI_STATE.lock() {
        state.about_visible = true;
        if let Some(ctx) = &state.ctx {
            ctx.request_repaint();
        }
    }
}

pub fn shutdown() {
    if let Ok(mut state) = UI_STATE.lock() {
        state.should_exit = true;
        if let Some(ctx) = &state.ctx {
            ctx.request_repaint();
        }
    }
}

pub struct OverlayApp;

impl OverlayApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        if let Ok(mut state) = UI_STATE.lock() {
            state.ctx = Some(cc.egui_ctx.clone());
            if let Some(visuals) = &state.theme {
                cc.egui_ctx.set_visuals(visuals.clone());
            }
        }

        Self
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
                if let Some(last) = state.last_update {
                    if last.elapsed() > Duration::from_secs(2) {
                        if state.brightness_visible {
                            println!("mhd: UI: brightness timeout, hiding");
                            state.brightness_visible = false;
                        }
                    }
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

        if about_visible {
            show_about_viewport(ctx, theme.clone());
        }

        if brightness_visible {
            show_brightness_viewport(ctx, brightness_value, &monitor_name, ctx.pixels_per_point(), theme.clone());
        }

        // Keep the main loop alive to process timeouts and commands
        ctx.request_repaint_after(Duration::from_millis(if brightness_visible { 16 } else { 100 }));
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Color32::TRANSPARENT.to_normalized_gamma_f32()
    }
}

fn show_about_viewport(parent_ctx: &egui::Context, theme: Option<egui::Visuals>) {
    parent_ctx.show_viewport_deferred(
        egui::ViewportId::from_hash_of("mhd_about"),
        egui::ViewportBuilder::default()
            .with_title("About mhd")
            .with_inner_size(egui::vec2(380.0, 250.0))
            .with_min_inner_size(egui::vec2(360.0, 230.0))
            .with_resizable(false)
            .with_decorations(true)
            .with_always_on_top()
            .with_active(true),
        move |ctx, _class| {
            if let Some(visuals) = &theme {
                ctx.set_visuals(visuals.clone());
            }

            if ctx.input(|i| i.viewport().close_requested()) {
                if let Ok(mut state) = UI_STATE.lock() {
                    state.about_visible = false;
                }
                return;
            }

            egui::CentralPanel::default().show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(18.0);
                    ui.heading(egui::RichText::new("mhd").size(30.0).strong());
                    ui.add_space(6.0);
                    ui.label("Mouse & Hotkey Daemon for Windows");
                    ui.add_space(12.0);
                    ui.label("Lightweight, single-binary, DDC/CI support.");
                    ui.add_space(22.0);

                    if ui.button("OK").clicked() {
                        if let Ok(mut state) = UI_STATE.lock() {
                            state.about_visible = false;
                        }
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
        },
    );
}

fn show_brightness_viewport(
    parent_ctx: &egui::Context,
    brightness_value: u32,
    monitor_name: &str,
    pixels_per_point: f32,
    theme: Option<egui::Visuals>,
) {
    let screen_width = unsafe { GetSystemMetrics(SM_CXSCREEN) } as f32 / pixels_per_point;
    let screen_height = unsafe { GetSystemMetrics(SM_CYSCREEN) } as f32 / pixels_per_point;
    
    // Proportions matching the screenshot, halved + adjusted for clipping
    let window_size = egui::vec2(210.0, 50.0);
    let margin = egui::vec2(24.0, 34.0); // Lifted by 10px

    parent_ctx.show_viewport_immediate(
        egui::ViewportId::from_hash_of("mhd_brightness"),
        egui::ViewportBuilder::default()
            .with_title("mhd_brightness")
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(false)
            .with_taskbar(false)
            .with_active(false)
            .with_mouse_passthrough(true)
            .with_always_on_top()
            .with_position(egui::pos2(
                screen_width - window_size.x - margin.x,
                screen_height - window_size.y - margin.y,
            ))
            .with_inner_size(window_size),
        move |ctx, _class| {
            if let Some(visuals) = &theme {
                ctx.set_visuals(visuals.clone());
            }

            let panel_frame = egui::Frame::window(&ctx.style())
                .fill(ctx.style().visuals.window_fill)
                .stroke(ctx.style().visuals.window_stroke)
                .corner_radius(egui::CornerRadius::same(3))
                .inner_margin(egui::Margin {
                    left: 8,
                    right: 8,
                    top: 6,
                    bottom: 6,
                });

            egui::CentralPanel::default()
                .frame(panel_frame)
                .show(ctx, |ui| {
                    // Monitor Name
                    ui.label(
                        egui::RichText::new(monitor_name)
                            .color(ui.visuals().text_color())
                            .size(8.0),
                    );

                    ui.add_space(6.0);

                    // Slider Track Row
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        // The text takes some space, we allocate the rest for the track
                        let text_width = 24.0;
                        let spacing = 6.0;
                        let available_width = ui.available_width() - text_width - spacing;
                        
                        let thumb_height = 14.0;
                        let (rect, _response) = ui.allocate_exact_size(
                            egui::vec2(available_width, thumb_height),
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

                        // Thumb block (tall and narrow like the screenshot)
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
        },
    );
}

pub fn run_ui_thread() {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("mhd_overlay")
            .with_decorations(false)
            .with_transparent(true)
            .with_active(false)
            .with_mouse_passthrough(true)
            .with_taskbar(false)
            .with_position(egui::pos2(0.0, 0.0))
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
