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
    pub last_update: Option<Instant>,
    pub about_visible: bool,
    pub theme: Option<egui::Visuals>,
}

pub fn show_brightness(value: u32) {
    println!("mhd: UI: show_brightness({})", value);
    let mut state = match UI_STATE.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    state.brightness_value = value;
    state.brightness_visible = true;
    state.last_update = Some(Instant::now());
}

pub fn show_about() {
    println!("mhd: UI: show_about()");
    if let Ok(mut state) = UI_STATE.lock() {
        state.about_visible = true;
    }
}

pub struct OverlayApp;

impl OverlayApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        if let Ok(state) = UI_STATE.lock() {
            if let Some(visuals) = &state.theme {
                cc.egui_ctx.set_visuals(visuals.clone());
            }
        }

        Self
    }
}

impl eframe::App for OverlayApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let (brightness_visible, brightness_value, about_visible, theme) = {
            let mut state = match UI_STATE.lock() {
                Ok(s) => s,
                Err(_) => return,
            };

            if let Some(last) = state.last_update {
                if last.elapsed() > Duration::from_secs(2) {
                    state.brightness_visible = false;
                }
            }

            (
                state.brightness_visible,
                state.brightness_value,
                state.about_visible,
                state.theme.clone(),
            )
        };

        if about_visible {
            show_about_viewport(ctx, theme.clone());
        }

        if brightness_visible {
            show_brightness_viewport(ctx, brightness_value, ctx.pixels_per_point());
        }

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
    pixels_per_point: f32,
) {
    let screen_width = unsafe { GetSystemMetrics(SM_CXSCREEN) } as f32 / pixels_per_point;
    let screen_height = unsafe { GetSystemMetrics(SM_CYSCREEN) } as f32 / pixels_per_point;
    let window_size = egui::vec2(260.0, 90.0);
    let margin = egui::vec2(48.0, 48.0);

    parent_ctx.show_viewport_deferred(
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
        egui::CentralPanel::default().show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_size(egui::vec2(220.0, 70.0));
            ui.vertical_centered(|ui| {
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(format!("Brightness: {}%", brightness_value))
                                .strong()
                                .size(16.0),
                        );
                        ui.add_space(4.0);
                        ui.add(
                            egui::ProgressBar::new(brightness_value as f32 / 100.0)
                                .animate(true)
                                .rounding(egui::Rounding::same(4.0)),
                        );
            });
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
