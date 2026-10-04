use std::collections::HashMap;
use std::net::{TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::{bounded, Receiver, Sender};
use eframe::egui;
use tracing::{error, info, warn};

use crate::codec::{VideoBackend, VideoDecoder};
use crate::crypto::{derive_key, Cipher};
use crate::proto::{ControlMsg, InboundVideo};
use crate::transport::{recv_salt, ControlChannel};

/// A decoded RGBA frame ready for display
pub struct RgbaFrame {
    pub data: Vec<u8>, // RGBA flat bytes
    pub width: u32,
    pub height: u32,
}

/// Handle returned by spawn_threads — holds channels and stop flag for GUI integration
pub struct ViewerHandle {
    pub frame_rx: Receiver<RgbaFrame>,
    pub remote_w: u32,
    pub remote_h: u32,
    pub stop: Arc<AtomicBool>,
}

/// Connects + handshakes synchronously, then spawns decode pipeline threads.
/// Returns ViewerHandle with channels. Setting stop=true causes all threads to exit.
pub fn spawn_threads(
    host: &str,
    port: u16,
    password: &str,
    backend: VideoBackend,
    ctx: egui::Context,
) -> Result<ViewerHandle> {
    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr).context("connect to host")?;
    stream.set_nodelay(true)?;
    stream
        .set_read_timeout(Some(Duration::from_secs(8)))
        .context("set handshake timeout")?;
    info!("Connected to {addr}");

    // Bind our UDP video socket first so we can tell the host which port to stream
    // to (per-connection, so one viewer can receive from multiple hosts at once).
    let udp_sock = UdpSocket::bind("0.0.0.0:0").context("bind UDP video socket")?;
    let udp_port = udp_sock.local_addr().context("UDP local addr")?.port();

    // Encryption handshake: read the host's salt, derive the shared key.
    let salt = recv_salt(&mut stream)?;
    let key = derive_key(password, &salt)?;
    let cipher = Cipher::new(&key);

    let mut ctrl = ControlChannel::new(stream, cipher.clone());
    ctrl.send(&ControlMsg::Hello { udp_port })?;

    let (remote_w, remote_h, fps) = match ctrl.recv() {
        Ok(ControlMsg::Welcome { width, height, fps }) => (width, height, fps),
        Ok(other) => anyhow::bail!("expected Welcome, got {other:?}"),
        Err(_) => anyhow::bail!("Incorrect password, or the host rejected the connection"),
    };
    info!("Remote screen {remote_w}×{remote_h} @ {fps} fps");
    ctrl.try_clone_stream()?
        .set_read_timeout(None)
        .context("clear handshake timeout")?;

    let stop = Arc::new(AtomicBool::new(false));

    // Channel: assembled NAL data → decoder
    let (nal_tx, nal_rx) = bounded::<Vec<u8>>(4);
    // Channel: decoded RGBA frames → GUI
    let (frame_tx, frame_rx) = bounded::<RgbaFrame>(2);

    // UDP receiver thread — decrypts datagrams, assembles chunks into complete NALs
    {
        let stop = stop.clone();
        let cipher = cipher.clone();
        std::thread::Builder::new()
            .name("udp-recv".into())
            .spawn(move || udp_receiver(udp_sock, cipher, nal_tx, stop))?;
    }

    #[cfg(target_os = "windows")]
    if backend == VideoBackend::Gpu {
        use openipc_video::{VideoCodec, VideoDecoder as _};
        let caps = openipc_video::PlatformDecoder::probe_capabilities();
        let h264 = caps
            .codec(VideoCodec::H264)
            .ok_or_else(|| anyhow::anyhow!("GPU decoder reports no H.264 capability"))?;
        if !h264.supported {
            anyhow::bail!("GPU H.264 decode is not supported by the selected Windows adapter");
        }
        if !h264.hardware_accelerated {
            anyhow::bail!("Windows H.264 decoder is not hardware accelerated on the selected adapter");
        }
    }

    // Decoder thread. Media Foundation decoder objects are thread-affine/non-Send,
    // so construct and drive the decoder on this worker thread.
    {
        let stop = stop.clone();
        let frame_tx2 = frame_tx;
        let ctx2 = ctx.clone();
        std::thread::Builder::new()
            .name("decoder".into())
            .spawn(move || {
                let mut decoder = match VideoDecoder::new(backend, fps) {
                    Ok(d) => d,
                    Err(e) => {
                        warn!("Decoder init: {e:#}");
                        return;
                    }
                };
                loop {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    match nal_rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(nal) => match decoder.decode(&nal) {
                            Ok(Some((data, w, h))) => {
                                frame_tx2
                                    .try_send(RgbaFrame {
                                        data,
                                        width: w,
                                        height: h,
                                    })
                                    .ok();
                                ctx2.request_repaint();
                            }
                            Ok(None) => {}
                            Err(e) => warn!("Decode error: {e}"),
                        },
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })?;
    }

    // View-only control channel. Keep a reader alive so disconnects are detected,
    // but do not expose any channel for keyboard/mouse/clipboard/file messages.
    let mut ctrl_reader = ctrl.try_clone()?;

    {
        let stop = stop.clone();
        let shutdown_stream = ctrl.try_clone_stream()?;
        std::thread::Builder::new()
            .name("ctrl-shutdown".into())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                let _ = shutdown_stream.shutdown(std::net::Shutdown::Both);
            })?;
    }

    {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("ctrl-recv".into())
            .spawn(move || loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                match ctrl_reader.recv() {
                    Ok(_) => {}
                    Err(_) => {
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            })?;
    }

    Ok(ViewerHandle {
        frame_rx,
        remote_w,
        remote_h,
        stop,
    })
}

/// Receive UDP datagrams, decrypt them, reassemble chunks into complete H.264 NALs
fn udp_receiver(sock: UdpSocket, cipher: Cipher, nal_tx: Sender<Vec<u8>>, stop: Arc<AtomicBool>) {
    sock.set_read_timeout(Some(Duration::from_millis(200))).ok();
    let local_port = sock.local_addr().map(|a| a.port()).unwrap_or(0);
    info!("UDP video receiver on port {local_port}");

    // frame_id → (expected_chunks, chunks_received: HashMap<chunk_idx, data>)
    let mut pending: HashMap<u32, (u16, HashMap<u16, Vec<u8>>)> = HashMap::new();
    let mut buf = vec![0u8; 65536];
    let mut last_seen_id: u32 = 0;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => {
                warn!("UDP recv: {e}");
                continue;
            }
        };

        // Decrypt, then parse. Drop anything that fails to authenticate.
        let Some(plain) = cipher.open(&buf[..n]) else {
            continue;
        };
        let Some(pkt) = InboundVideo::parse(&plain) else {
            continue;
        };

        // Drop frames we've already passed
        if pkt.frame_id.wrapping_sub(last_seen_id) > 60 {
            continue;
        }

        let entry = pending
            .entry(pkt.frame_id)
            .or_insert_with(|| (pkt.total_chunks, HashMap::new()));

        entry.1.insert(pkt.chunk_idx, pkt.data);

        if entry.1.len() == entry.0 as usize {
            // All chunks received — reassemble in order
            let total = entry.0;
            let chunks = pending.remove(&pkt.frame_id).unwrap().1;
            let mut assembled = Vec::new();
            for i in 0..total {
                if let Some(d) = chunks.get(&i) {
                    assembled.extend_from_slice(d);
                }
            }
            last_seen_id = pkt.frame_id;
            nal_tx.try_send(assembled).ok();

            // Evict any stale pending frames older than this one
            pending.retain(|&id, _| id.wrapping_sub(last_seen_id) < 120);
        }
    }
}

// ─── CLI run path ──────────────────────────────────────────────────────────────

/// Used by the CLI `view` subcommand — opens its own eframe window
pub fn run(host: &str, port: u16, password: &str, backend: VideoBackend) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Rust P2P Viewer")
            .with_inner_size([1280.0, 720.0])
            .with_resizable(true),
        ..Default::default()
    };

    let host = host.to_string();
    let password = password.to_string();
    eframe::run_native(
        "Rust P2P Viewer",
        options,
        Box::new(move |cc| {
            let handle = spawn_threads(&host, port, &password, backend, cc.egui_ctx.clone())
                .expect("Failed to connect to host");
            Ok(Box::new(ViewerWindow::new(handle)) as Box<dyn eframe::App>)
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// eframe App that renders the remote screen in strict view-only mode
pub struct ViewerWindow {
    handle: ViewerHandle,
    texture: Option<egui::TextureHandle>,
    screen_rect: egui::Rect,
}

impl ViewerWindow {
    pub fn new(handle: ViewerHandle) -> Self {
        Self {
            handle,
            texture: None,
            screen_rect: egui::Rect::ZERO,
        }
    }
}

impl eframe::App for ViewerWindow {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Drain latest frame from decoder
        while let Ok(frame) = self.handle.frame_rx.try_recv() {
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [frame.width as usize, frame.height as usize],
                &frame.data,
            );
            match self.texture.as_mut() {
                Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                None => {
                    self.texture = Some(ctx.load_texture(
                        "remote_screen",
                        image,
                        egui::TextureOptions::LINEAR,
                    ));
                }
            }
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(egui::Color32::BLACK))
            .show_inside(ui, |ui| {
                if let Some(tex) = &self.texture {
                    self.screen_rect = crate::gui::paint_remote(ui, tex);
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(egui::RichText::new("Connecting…").color(egui::Color32::WHITE));
                    });
                }
            });

        // Keep redrawing so newly decoded frames appear without an input event.
        ctx.request_repaint();
    }

    fn on_exit(&mut self) {
        self.handle.stop.store(true, Ordering::Relaxed);
    }
}
