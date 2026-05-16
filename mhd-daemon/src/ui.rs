use std::net::UdpSocket;
use std::process::Child;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use eframe::egui;
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

lazy_static::lazy_static! {
    static ref UI_PROCESS: Mutex<Option<Child>> = Mutex::new(None);
}

pub fn show_brightness(value: u32, name: String) {
    send_ipc(&format!("B:{value}:{name}"));
}

pub fn show_about() {
    send_ipc("A");
}

pub fn shutdown() {
    send_ipc("Q");
}

fn send_ipc(msg: &str) {
    let mut child_guard = match UI_PROCESS.lock() {
        Ok(g) => g,
        Err(_) => return,
    };

    let is_running = child_guard.as_mut().map_or(false, |c| match c.try_wait() {
        Ok(None) => true,
        _ => false,
    });

    if !is_running {
        if let Ok(exe) = std::env::current_exe() {
            if let Ok(child) = std::process::Command::new(exe).arg("--ui-server").spawn() {
                *child_guard = Some(child);
            }
        }
        std::thread::sleep(Duration::from_millis(250)); // Wait for eframe/wgpu initialization
    }

    if let Ok(sock) = UdpSocket::bind("127.0.0.1:0") {
        let _ = sock.send_to(msg.as_bytes(), "127.0.0.1:34254");
    }
}

pub fn run_ui_server() {
    let config_path = crate::resolve_config_path();
    let config = crate::config::Config::load(&config_path).unwrap_or_default();
    let theme = config.theme.as_deref().and_then(|t| crate::theme::load_theme(t, &config_path));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("mhd_overlay")
            .with_decorations(false)
            .with_transparent(true)
            .with_taskbar(false)
            .with_always_on_top(),
        ..Default::default()
    };

    let _ = eframe::run_native(
        "mhd_overlay",
        options,
        Box::new(move |cc| {
            if let Some(t) = &theme {
                cc.egui_ctx.set_visuals(t.clone());
            }
            Ok(Box::new(UiServerApp::new()))
        }),
    );
}

struct UiServerApp {
    sock: Option<UdpSocket>,
    brightness_visible: bool,
    brightness_value: u32,
    monitor_name: String,
    about_visible: bool,
    last_update: Instant,
}

impl UiServerApp {
    fn new() -> Self {
        // Try to bind, if it fails (another instance running?), we will just gracefully exit later
        let sock = UdpSocket::bind("127.0.0.1:34254").ok();
        if let Some(s) = &sock {
            let _ = s.set_nonblocking(true);
        }

        Self {
            sock,
            brightness_visible: false,
            brightness_value: 0,
            monitor_name: String::new(),
            about_visible: false,
            last_update: Instant::now(),
        }
    }
}

impl eframe::App for UiServerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(sock) = &self.sock {
            let mut buf = [0u8; 1024];
            while let Ok((len, _)) = sock.recv_from(&mut buf) {
                if let Ok(msg) = std::str::from_utf8(&buf[..len]) {
                    if msg == "A" {
                        self.about_visible = true;
                        self.brightness_visible = false;
                        self.last_update = Instant::now();
                    } else if msg == "Q" {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        return;
                    } else if msg.starts_with("B:") {
                        let parts: Vec<&str> = msg.splitn(3, ':').collect();
                        if parts.len() == 3 {
                            if let Ok(v) = parts[1].parse() {
                                self.brightness_value = v;
                                self.monitor_name = parts[2].to_string();
                                self.brightness_visible = true;
                                self.about_visible = false;
                                self.last_update = Instant::now();
                            }
                        }
                    }
                }
            }
        } else {
            // Socket failed to bind, abort
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        if self.brightness_visible && self.last_update.elapsed() > Duration::from_secs(2) {
            self.brightness_visible = false;
        }

        if !self.brightness_visible && !self.about_visible && self.last_update.elapsed() > Duration::from_millis(500) {
            // Auto-terminate when nothing is displayed to free 100% of CPU and RAM
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        if self.about_visible {
            show_about_ui(ctx);
        } else if self.brightness_visible {
            show_brightness_ui(ctx, self.brightness_value, &self.monitor_name);
        }

        // Keep polling UDP messages while the window is alive
        ctx.request_repaint_after(Duration::from_millis(32));
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Color32::TRANSPARENT.to_normalized_gamma_f32()
    }
}

fn show_about_ui(ctx: &egui::Context) {
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

    if ctx.input(|i| i.pointer.primary_clicked() || i.viewport().close_requested()) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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

fn show_brightness_ui(ctx: &egui::Context, brightness_value: u32, monitor_name: &str) {
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
        ui.label(
            egui::RichText::new(monitor_name)
                .color(ui.visuals().text_color())
                .size(8.0),
        );

        ui.add_space(6.0);

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

            let track_height = 1.0;
            let track_rect = egui::Rect::from_min_size(
                egui::pos2(rect.min.x, rect.center().y - track_height / 2.0),
                egui::vec2(rect.width(), track_height),
            );

            ui.painter().rect_filled(track_rect, 0.0, track_color);

            let filled_width = rect.width() * fraction;
            let filled_rect = egui::Rect::from_min_size(
                track_rect.min,
                egui::vec2(filled_width, track_height),
            );
            ui.painter().rect_filled(filled_rect, 0.0, active_color);

            let thumb_width = 4.0;
            let thumb_x = (track_rect.min.x + filled_width - thumb_width / 2.0)
                .clamp(rect.min.x, rect.max.x - thumb_width);

            let thumb_rect = egui::Rect::from_min_size(
                egui::pos2(thumb_x, rect.center().y - thumb_height / 2.0),
                egui::vec2(thumb_width, thumb_height),
            );
            ui.painter().rect_filled(thumb_rect, 0.0, active_color);

            ui.add_space(spacing);
            
            ui.label(
                egui::RichText::new(format!("{}", brightness_value))
                    .color(ui.visuals().text_color())
                    .size(8.0),
            );
        });
    });
}
