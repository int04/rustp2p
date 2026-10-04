use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use crossbeam_channel::Receiver;
use eframe::egui;

use crate::codec::VideoBackend;
use crate::viewer::ViewerHandle;

/// Accent color used across the UI (matches the app icon).
const ACCENT: egui::Color32 = egui::Color32::from_rgb(67, 196, 99);

pub fn run() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Rust P2P Viewer")
            .with_inner_size([460.0, 440.0])
            .with_min_inner_size([380.0, 360.0])
            .with_icon(std::sync::Arc::new(load_icon())),
        // Keep a single frame queued for presentation. This preserves tear-free
        // vsync while avoiding the extra 1-2 frames of latency from the default.
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            present_mode: eframe::wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: Some(1),
            ..Default::default()
        },
        dithering: false,
        ..Default::default()
    };

    eframe::run_native(
        "Rust P2P Viewer",
        options,
        Box::new(|cc| {
            setup_theme(&cc.egui_ctx);
            Ok(Box::new(App::default()) as Box<dyn eframe::App>)
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Window/dock icon — raw 64×64 RGBA generated at build time (no decoder needed).
fn load_icon() -> egui::IconData {
    let rgba = include_bytes!("../icons/icon_64.rgba").to_vec();
    egui::IconData {
        rgba,
        width: 64,
        height: 64,
    }
}

/// Apply a polished dark theme with the accent color and comfortable spacing.
fn setup_theme(ctx: &egui::Context) {
    use egui::{Color32, FontFamily, FontId, Stroke, TextStyle};

    let mut v = egui::Visuals::dark();
    v.panel_fill = Color32::from_rgb(27, 31, 40);
    v.window_fill = Color32::from_rgb(27, 31, 40);
    v.extreme_bg_color = Color32::from_rgb(17, 20, 27);
    v.selection.bg_fill = Color32::from_rgb(34, 64, 45);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;
    v.widgets.inactive.bg_fill = Color32::from_rgb(38, 43, 54);
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(38, 43, 54);
    v.widgets.hovered.bg_fill = Color32::from_rgb(48, 54, 67);
    ctx.set_visuals(v);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    style.spacing.slider_width = 150.0;
    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(26.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(15.0, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(15.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new(12.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(14.0, FontFamily::Monospace),
        ),
    ]
    .into();
    ctx.set_style(style);
}

// ─── Tab ────────────────────────────────────────────────────────────────────

#[derive(PartialEq)]
enum Tab {
    Host,
    Connect,
}

// ─── Host state ───────────────────────────────────────────────────────────────

enum HostState {
    Idle,
    Hosting {
        status: Arc<Mutex<String>>,
        stop: Arc<AtomicBool>,
    },
}

// ─── Pending connection ─────────────────────────────────────────────────────

/// A connection attempt in flight (handshake runs on a worker thread).
struct Pending {
    label: String,
    rx: Receiver<Result<ViewerHandle>>,
}

// ─── Session ──────────────────────────────────────────────────────────────────

/// An active viewer session, rendered in its own OS window (egui viewport).
struct Session {
    id: egui::ViewportId,
    label: String,
    handle: ViewerHandle,
    texture: Option<egui::TextureHandle>,
    screen_rect: egui::Rect,
}

// ─── App ──────────────────────────────────────────────────────────────────────

struct App {
    tab: Tab,
    // Host settings
    fps: u32,
    bitrate: u32,
    host_backend: VideoBackend,
    host_password: String,
    local_ip: String,
    host: HostState,
    // Connect form
    host_ip: String,
    view_password: String,
    view_backend: VideoBackend,
    connect_error: Option<String>,
    // Multi-session state
    pending: Vec<Pending>,
    sessions: Vec<Session>,
    next_session: u64,
}

impl Default for App {
    fn default() -> Self {
        Self {
            tab: Tab::Connect,
            fps: 60,
            bitrate: 8,
            host_backend: VideoBackend::Cpu,
            host_password: String::new(),
            local_ip: detect_local_ip(),
            host: HostState::Idle,
            host_ip: String::new(),
            view_password: String::new(),
            view_backend: VideoBackend::Cpu,
            connect_error: None,
            pending: Vec::new(),
            sessions: Vec::new(),
            next_session: 0,
        }
    }
}

fn detect_local_ip() -> String {
    use std::net::UdpSocket;
    let sock = UdpSocket::bind("0.0.0.0:0").ok();
    if let Some(s) = sock {
        if s.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = s.local_addr() {
                return addr.ip().to_string();
            }
        }
    }
    "unknown".to_string()
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Promote any finished connection attempts into live session windows.
        self.poll_pending(&ctx);

        // Main control window.
        self.render_main(ui);

        // Each active session renders in its own OS window.
        self.render_sessions(&ctx);

        // Keep the event loop ticking while anything is live so frames update.
        if !self.sessions.is_empty() || !self.pending.is_empty() {
            ctx.request_repaint();
        }
    }

    fn on_exit(&mut self) {
        if let HostState::Hosting { stop, .. } = &self.host {
            stop.store(true, Ordering::Relaxed);
        }
        for s in &self.sessions {
            s.handle.stop.store(true, Ordering::Relaxed);
        }
    }
}

impl App {
    fn poll_pending(&mut self, ctx: &egui::Context) {
        let mut still = Vec::new();
        for p in std::mem::take(&mut self.pending) {
            match p.rx.try_recv() {
                Ok(Ok(handle)) => {
                    let id = self.next_session;
                    self.next_session += 1;
                    self.sessions.push(Session {
                        id: egui::ViewportId::from_hash_of(("rpv-session", id)),
                        label: p.label.clone(),
                        handle,
                        texture: None,
                        screen_rect: egui::Rect::ZERO,
                    });
                    self.connect_error = None;
                    ctx.request_repaint();
                }
                Ok(Err(e)) => {
                    self.connect_error = Some(format!("Connect to {} failed: {e:#}", p.label));
                }
                Err(crossbeam_channel::TryRecvError::Empty) => still.push(p),
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    self.connect_error = Some(format!("Connect to {} failed", p.label));
                }
            }
        }
        self.pending = still;
    }

    fn render_main(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.add_space(16.0);

            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("Rust P2P Viewer").heading().strong());
                ui.label(
                    egui::RichText::new("Direct LAN · Low Latency")
                        .small()
                        .color(egui::Color32::from_gray(140)),
                );
            });

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(12.0);

            ui.vertical_centered(|ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let tab_size = egui::vec2(110.0, 30.0);
                    if ui
                        .add_sized(
                            tab_size,
                            egui::SelectableLabel::new(self.tab == Tab::Connect, "CONNECT"),
                        )
                        .clicked()
                    {
                        self.tab = Tab::Connect;
                    }
                    if ui
                        .add_sized(
                            tab_size,
                            egui::SelectableLabel::new(self.tab == Tab::Host, "HOST"),
                        )
                        .clicked()
                    {
                        self.tab = Tab::Host;
                    }
                });
            });

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(16.0);

            match self.tab {
                Tab::Host => self.show_host_tab(ui),
                Tab::Connect => self.show_connect_tab(ui),
            }
        });
    }

    fn show_host_tab(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("host_grid")
            .num_columns(2)
            .spacing([16.0, 8.0])
            .show(ui, |ui| {
                ui.label("Your LAN IP:");
                ui.label(
                    egui::RichText::new(&self.local_ip)
                        .monospace()
                        .color(ACCENT),
                );
                ui.end_row();

                ui.label("Password:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.host_password)
                        .password(true)
                        .hint_text("required to connect")
                        .desired_width(160.0),
                );
                ui.end_row();

                ui.label("FPS:");
                egui::ComboBox::from_id_salt("host_fps")
                    .selected_text(format!("{} FPS", self.fps))
                    .show_ui(ui, |ui| {
                        for fps in [30_u32, 60, 70, 90, 120, 150, 240] {
                            ui.selectable_value(&mut self.fps, fps, format!("{fps} FPS"));
                        }
                    });
                ui.end_row();

                ui.label("Bitrate:");
                ui.add(egui::Slider::new(&mut self.bitrate, 1..=50).suffix(" Mbps"));
                ui.end_row();

                ui.label("Encoder:");
                egui::ComboBox::from_id_salt("host_encoder")
                    .selected_text(self.host_backend.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.host_backend, VideoBackend::Cpu, "CPU · OpenH264");
                        ui.selectable_value(&mut self.host_backend, VideoBackend::Gpu, "GPU · Hardware");
                    });
                ui.end_row();
            });

        ui.add_space(16.0);

        let hosting_stop: Option<Arc<AtomicBool>> = match &self.host {
            HostState::Hosting { stop, .. } => Some(stop.clone()),
            HostState::Idle => None,
        };

        ui.vertical_centered(|ui| {
            if let Some(stop) = hosting_stop {
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new("⬛  Stop").strong())
                            .fill(egui::Color32::from_rgb(200, 70, 60))
                            .min_size(egui::vec2(200.0, 40.0)),
                    )
                    .clicked()
                {
                    stop.store(true, Ordering::Relaxed);
                    self.host = HostState::Idle;
                }
            } else if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("▶  Start Hosting")
                            .color(egui::Color32::from_rgb(15, 22, 16))
                            .strong(),
                    )
                    .fill(ACCENT)
                    .min_size(egui::vec2(200.0, 40.0)),
                )
                .clicked()
            {
                let status = Arc::new(Mutex::new("Starting…".to_string()));
                let stop = Arc::new(AtomicBool::new(false));
                let status_clone = status.clone();
                let stop_clone = stop.clone();
                let fps = self.fps;
                let bitrate = self.bitrate;
                let backend = self.host_backend;
                let password = self.host_password.clone();

                std::thread::Builder::new()
                    .name("host-run".into())
                    .spawn(move || {
                        if let Err(e) = crate::host::run_with_stop(
                            "0.0.0.0",
                            7272,
                            fps,
                            bitrate,
                            backend,
                            password,
                            status_clone,
                            stop_clone,
                        ) {
                            tracing::error!("Host error: {e:#}");
                        }
                    })
                    .ok();

                self.host = HostState::Hosting { status, stop };
            }
        });

        ui.add_space(12.0);

        ui.vertical_centered(|ui| match &self.host {
            HostState::Hosting { status, .. } => {
                let s = status.lock().unwrap().clone();
                ui.label(egui::RichText::new(format!("● {s}")).color(ACCENT).small());
            }
            HostState::Idle => {
                ui.label(
                    egui::RichText::new("● Idle")
                        .color(egui::Color32::GRAY)
                        .small(),
                );
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new("Share your IP + password with the viewer")
                        .small()
                        .color(egui::Color32::from_gray(120)),
                );
            }
        });
    }

    fn show_connect_tab(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("connect_grid")
            .num_columns(2)
            .spacing([16.0, 8.0])
            .show(ui, |ui| {
                ui.label("Host IP:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.host_ip)
                        .hint_text("192.168.1.x")
                        .desired_width(160.0),
                );
                ui.end_row();

                ui.label("Password:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.view_password)
                        .password(true)
                        .hint_text("host's password")
                        .desired_width(160.0),
                );
                ui.end_row();

                ui.label("Decoder:");
                egui::ComboBox::from_id_salt("view_decoder")
                    .selected_text(self.view_backend.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.view_backend, VideoBackend::Cpu, "CPU · OpenH264");
                        ui.selectable_value(&mut self.view_backend, VideoBackend::Gpu, "GPU · Hardware");
                    });
                ui.end_row();
            });

        ui.add_space(14.0);

        let can_connect = !self.host_ip.trim().is_empty();
        ui.vertical_centered(|ui| {
            let btn = egui::Button::new(
                egui::RichText::new("▶  Connect")
                    .color(egui::Color32::from_rgb(15, 22, 16))
                    .strong(),
            )
            .fill(ACCENT)
            .min_size(egui::vec2(200.0, 40.0));
            if ui.add_enabled(can_connect, btn).clicked() {
                self.start_connect(ui.ctx().clone());
            }
        });

        if let Some(err) = &self.connect_error {
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(err)
                        .color(egui::Color32::from_rgb(220, 80, 80))
                        .small(),
                );
            });
        }

        if !self.pending.is_empty() {
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(format!("Connecting… ({})", self.pending.len()))
                        .color(egui::Color32::from_gray(150))
                        .small(),
                );
            });
        }

        // Active session list with per-session disconnect.
        if !self.sessions.is_empty() {
            ui.add_space(12.0);
            ui.separator();
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(format!("Active sessions ({})", self.sessions.len())).strong(),
            );
            ui.add_space(4.0);

            let mut close: Vec<egui::ViewportId> = Vec::new();
            for s in &self.sessions {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("●").color(ACCENT));
                    ui.label(&s.label);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Disconnect").clicked() {
                            close.push(s.id);
                        }
                    });
                });
            }
            if !close.is_empty() {
                self.sessions.retain(|s| {
                    if close.contains(&s.id) {
                        s.handle.stop.store(true, Ordering::Relaxed);
                        false
                    } else {
                        true
                    }
                });
            }
        }
    }

    fn start_connect(&mut self, ctx: egui::Context) {
        self.connect_error = None;
        let ip = self.host_ip.trim().to_string();
        let pw = self.view_password.clone();
        let backend = self.view_backend;
        let (tx, rx) = crossbeam_channel::bounded(1);

        std::thread::Builder::new()
            .name("viewer-connect".into())
            .spawn(move || {
                let result = crate::viewer::spawn_threads(&ip, 7272, &pw, backend, ctx);
                tx.send(result).ok();
            })
            .ok();

        self.pending.push(Pending {
            label: self.host_ip.trim().to_string(),
            rx,
        });
    }

    fn render_sessions(&mut self, ctx: &egui::Context) {
        // Take the list out so each session can be borrowed mutably inside the
        // viewport callback without conflicting with `self`.
        let mut sessions = std::mem::take(&mut self.sessions);
        sessions.retain_mut(|s| render_one_session(ctx, s));
        // Preserve any sessions that were created while we were rendering.
        self.sessions.append(&mut sessions);
    }
}

/// Render a single session in its own OS window. Returns `false` if the window
/// was closed (so the caller drops the session).
fn render_one_session(ctx: &egui::Context, s: &mut Session) -> bool {
    // Pull the newest decoded frames into the texture (done outside the viewport
    // callback so the borrow of `s` stays simple).
    while let Ok(frame) = s.handle.frame_rx.try_recv() {
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize],
            &frame.data,
        );
        match s.texture.as_mut() {
            Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
            None => {
                s.texture = Some(ctx.load_texture(
                    format!("remote-{:?}", s.id),
                    image,
                    egui::TextureOptions::LINEAR,
                ));
            }
        }
    }

    let builder = egui::ViewportBuilder::default()
        .with_title(format!("{} — Rust P2P Viewer", s.label))
        .with_inner_size([1280.0, 720.0]);

    let mut keep = true;
    ctx.show_viewport_immediate(s.id, builder, |vctx, _class| {
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(egui::Color32::BLACK))
            .show(vctx, |ui| {
                if let Some(tex) = &s.texture {
                    s.screen_rect = paint_remote(ui, tex);
                } else {
                    ui.centered_and_justified(|ui| {
                        let status = s.handle.status.lock().unwrap().clone();
                        ui.label(egui::RichText::new(status).color(egui::Color32::WHITE));
                    });
                }
            });

        if vctx.input(|i| i.viewport().close_requested()) {
            keep = false;
        }
        vctx.request_repaint();
    });

    if !keep {
        s.handle.stop.store(true, Ordering::Relaxed);
    }
    keep
}

/// Paint a texture into the current UI, aspect-correct and centered, and return
/// the exact rect the image occupies (for input coordinate mapping).
pub(crate) fn paint_remote(ui: &mut egui::Ui, tex: &egui::TextureHandle) -> egui::Rect {
    let panel = ui.available_rect_before_wrap();
    let img = tex.size_vec2();
    if img.x <= 0.0 || img.y <= 0.0 {
        return panel;
    }
    let scale = (panel.width() / img.x).min(panel.height() / img.y);
    let draw = img * scale;
    let rect = egui::Rect::from_center_size(panel.center(), draw);
    ui.painter().image(
        tex.id(),
        rect,
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
    rect
}
