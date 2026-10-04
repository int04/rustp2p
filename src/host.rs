// Viewer-only (no-capture) build: provide stub host fns so the rest of the crate
// still compiles without the scap capture backend (and thus without full Xcode).
#[cfg(not(feature = "capture"))]
use anyhow::Result;
#[cfg(not(feature = "capture"))]
use crate::codec::VideoBackend;
#[cfg(not(feature = "capture"))]
use std::sync::atomic::AtomicBool;
#[cfg(not(feature = "capture"))]
use std::sync::{Arc, Mutex};

#[cfg(not(feature = "capture"))]
pub fn run(
    _bind: &str,
    _port: u16,
    _fps: u32,
    _bitrate_mbps: u32,
    _backend: VideoBackend,
    _password: &str,
) -> Result<()> {
    anyhow::bail!("This build was compiled without screen-capture support (viewer-only)")
}

#[cfg(not(feature = "capture"))]
pub fn run_with_stop(
    _bind: &str,
    _port: u16,
    _fps: u32,
    _bitrate_mbps: u32,
    _backend: VideoBackend,
    _password: String,
    status: Arc<Mutex<String>>,
    _stop: Arc<AtomicBool>,
) -> Result<()> {
    *status.lock().unwrap() = "No capture support in this build".to_string();
    Ok(())
}

#[cfg(feature = "capture")]
pub use imp::{run, run_with_stop};

#[cfg(feature = "capture")]
mod imp {
    use std::net::{TcpListener, UdpSocket};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use crossbeam_channel::bounded;
    use scap::capturer::{Capturer, Options, Resolution};
    use scap::frame::{Frame, FrameType, VideoFrame};
    use tracing::{error, info, warn};

    use crate::codec::{VideoBackend, VideoEncoder};
    use crate::crypto::{derive_key, random_bytes, Cipher, SALT_LEN};
    use crate::proto::{ControlMsg, VideoPacket, VIDEO_CHUNK_MAX};
    use crate::transport::{send_salt, ControlChannel};

    pub fn run(
        bind: &str,
        port: u16,
        fps: u32,
        bitrate_mbps: u32,
        backend: VideoBackend,
        password: &str,
    ) -> Result<()> {
        if !scap::is_supported() {
            anyhow::bail!("Screen capture not supported on this platform");
        }
        #[cfg(target_os = "macos")]
        if !scap::has_permission() {
            eprintln!("Screen Recording permission required.");
            eprintln!("Grant it in System Settings → Privacy & Security → Screen Recording");
            scap::request_permission();
            std::process::exit(1);
        }

        let listener = TcpListener::bind(format!("{bind}:{port}")).context("bind TCP")?;
        info!("Listening on {bind}:{port}");
        info!("Run `rust-p2p-viewer view <this_ip>` on the viewer machine");

        for incoming in listener.incoming() {
            match incoming {
                Ok(stream) => {
                    let peer = stream.peer_addr()?;
                    let viewer_ip = peer.ip().to_string();
                    let password = password.to_owned();
                    info!("Viewer connected from {peer}");
                    std::thread::Builder::new()
                        .name(format!("session-{peer}"))
                        .spawn(move || {
                            if let Err(e) = handle_session(
                                stream,
                                viewer_ip,
                                fps,
                                bitrate_mbps,
                                backend,
                                &password,
                            ) {
                                error!("Session {peer} error: {e:#}");
                            }
                            info!("Viewer {peer} disconnected");
                        })?;
                }
                Err(e) => error!("Accept error: {e}"),
            }
        }
        Ok(())
    }

    /// Like `run()` but non-blocking accept loop with stop-flag support for GUI integration.
    pub fn run_with_stop(
        bind: &str,
        port: u16,
        fps: u32,
        bitrate_mbps: u32,
        backend: VideoBackend,
        password: String,
        status: Arc<Mutex<String>>,
        stop: Arc<AtomicBool>,
    ) -> Result<()> {
        if !scap::is_supported() {
            anyhow::bail!("Screen capture not supported on this platform");
        }
        #[cfg(target_os = "macos")]
        if !scap::has_permission() {
            *status.lock().unwrap() =
                "Screen Recording permission required. Grant in System Settings.".to_string();
            scap::request_permission();
            return Ok(());
        }

        let listener = TcpListener::bind(format!("{bind}:{port}")).context("bind TCP")?;
        listener.set_nonblocking(true).context("set nonblocking")?;
        info!("Listening on {bind}:{port} (GUI host mode)");
        *status.lock().unwrap() = "Waiting for viewer…".to_string();

        loop {
            if stop.load(Ordering::Relaxed) {
                *status.lock().unwrap() = "Stopped".to_string();
                return Ok(());
            }

            match listener.accept() {
                Ok((stream, peer)) => {
                    info!("Viewer connected from {peer}");
                    *status.lock().unwrap() = format!("Viewer connected: {peer}");

                    // Each viewer gets its own session thread. The accept loop must stay
                    // responsive so a stale/slow connection cannot block new viewers.
                    stream.set_nonblocking(false).ok();
                    let viewer_ip = peer.ip().to_string();
                    let session_password = password.clone();
                    std::thread::Builder::new()
                        .name(format!("session-{peer}"))
                        .spawn(move || {
                            if let Err(e) = handle_session(
                                stream,
                                viewer_ip,
                                fps,
                                bitrate_mbps,
                                backend,
                                &session_password,
                            ) {
                                error!("Session {peer} error: {e:#}");
                            }
                            info!("Viewer {peer} disconnected");
                        })?;

                    *status.lock().unwrap() = "Waiting for viewer…".to_string();
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    error!("Accept error: {e}");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    fn handle_session(
        mut stream: std::net::TcpStream,
        viewer_ip: String,
        fps: u32,
        bitrate_mbps: u32,
        backend: VideoBackend,
        password: &str,
    ) -> Result<()> {
        stream.set_nodelay(true)?;
        stream
            .set_read_timeout(Some(Duration::from_secs(8)))
            .context("set handshake timeout")?;

        // Encryption handshake: send a fresh salt, derive the shared key from the
        // password. A wrong password produces a different key, so the encrypted Hello
        // below will fail to decrypt and the connection is rejected.
        let salt = random_bytes::<SALT_LEN>();
        send_salt(&mut stream, &salt)?;
        let key = derive_key(password, &salt)?;
        let cipher = Cipher::new(&key);
        let mut ctrl = ControlChannel::new(stream, cipher.clone());

        let viewer_udp_port = match ctrl.recv() {
            Ok(ControlMsg::Hello { udp_port }) => udp_port,
            Ok(other) => anyhow::bail!("expected Hello, got {other:?}"),
            Err(e) => anyhow::bail!("handshake failed (wrong password?): {e:#}"),
        };

        // Use get_output_frame_size to learn dimensions without capturing a frame
        let [width, height] = {
            let mut probe = Capturer::build(Options {
                fps,
                output_type: FrameType::BGRAFrame,
                output_resolution: Resolution::Captured,
                ..Default::default()
            })
            .map_err(|e| anyhow::anyhow!("capturer probe: {e:?}"))?;
            probe.get_output_frame_size()
        };
        ctrl.send(&ControlMsg::Welcome { width, height, fps })?;
        ctrl.try_clone_stream()?
            .set_read_timeout(None)
            .context("clear handshake timeout")?;
        info!("Screen size {width}×{height}, streaming at {fps} fps / {bitrate_mbps} Mbps / {}", backend.label());

        let session_stop = Arc::new(AtomicBool::new(false));
        let (frame_tx, frame_rx) = bounded::<Vec<u8>>(2);
        let (nal_tx, nal_rx) = bounded::<Vec<u8>>(4);

        let viewer_video_addr = format!("{viewer_ip}:{viewer_udp_port}");
        info!("Video target UDP {viewer_video_addr}");
        let (w, h) = (width as usize, height as usize);

        // Encoder thread
        std::thread::Builder::new()
            .name("encoder".into())
            .spawn(move || {
                let mut enc = match VideoEncoder::new(backend, fps, bitrate_mbps, w, h) {
                    Ok(e) => e,
                    Err(e) => {
                        error!("Encoder init: {e:#}");
                        return;
                    }
                };
                while let Ok(bgra) = frame_rx.recv() {
                    match enc.encode_bgra(&bgra, w, h) {
                        Ok(nal) if !nal.is_empty() => {
                            nal_tx.send(nal).ok();
                        }
                        Ok(_) => {}
                        Err(e) => warn!("Encode error: {e}"),
                    }
                }
            })?;

        // UDP sender thread — each datagram is AEAD-sealed before sending
        let udp_cipher = cipher.clone();
        std::thread::Builder::new()
            .name("udp-sender".into())
            .spawn(move || {
                let sock = UdpSocket::bind("0.0.0.0:0").expect("bind UDP sender");
                sock.connect(&viewer_video_addr)
                    .expect("connect UDP sender");
                let mut frame_id: u32 = 0;
                let t0 = Instant::now();
                let mut logged_first_frame = false;
                let mut pkt_buf = Vec::with_capacity(VIDEO_CHUNK_MAX + crate::proto::VIDEO_HDR_LEN);

                while let Ok(nal) = nal_rx.recv() {
                    let chunks: Vec<&[u8]> = nal.chunks(VIDEO_CHUNK_MAX).collect();
                    let total = chunks.len() as u16;
                    if !logged_first_frame {
                        info!("Sending first video frame: {} bytes in {} UDP chunks to {}", nal.len(), total, viewer_video_addr);
                        logged_first_frame = true;
                    }
                    let pts_ms = t0.elapsed().as_millis() as u32;

                    for (idx, chunk) in chunks.iter().enumerate() {
                        pkt_buf.clear();
                        VideoPacket {
                            frame_id,
                            chunk_idx: idx as u16,
                            total_chunks: total,
                            pts_ms,
                            data: chunk,
                        }
                        .write_to(&mut pkt_buf);
                        let sealed = udp_cipher.seal(&pkt_buf);
                        if let Err(e) = sock.send(&sealed) {
                            warn!("UDP send: {e}");
                        }
                    }
                    frame_id = frame_id.wrapping_add(1);
                }
            })?;

        // Capture thread — Capturer must be created on the thread that uses it
        let capture_stop = session_stop.clone();
        std::thread::Builder::new()
            .name("capture".into())
            .spawn(move || {
                let mut capturer = match Capturer::build(Options {
                    fps,
                    show_cursor: true,
                    show_highlight: false,
                    output_type: FrameType::BGRAFrame,
                    output_resolution: Resolution::Captured,
                    ..Default::default()
                }) {
                    Ok(c) => c,
                    Err(e) => {
                        error!("Capturer build: {e:?}");
                        return;
                    }
                };
                capturer.start_capture();

                while !capture_stop.load(Ordering::Relaxed) {
                    match capturer.get_next_frame() {
                        Ok(Frame::Video(VideoFrame::BGRA(f))) if !f.data.is_empty() => {
                            frame_tx.try_send(f.data).ok();
                        }
                        Ok(_) => {}
                        Err(e) => {
                            warn!("Capture error: {e}");
                            break;
                        }
                    }
                }
                capturer.stop_capture();
            })?;

        // View-only mode: the TCP channel is used only to keep the session alive
        // and detect disconnects. No keyboard, mouse, clipboard, or file-control
        // messages are accepted by the host.
        loop {
            match ctrl.recv() {
                Ok(ControlMsg::Ping) => {
                    ctrl.send(&ControlMsg::Pong).ok();
                }
                Ok(_) => {}
                Err(e) => {
                    info!("Control channel closed: {e}");
                    break;
                }
            }
        }
        session_stop.store(true, Ordering::Relaxed);
        Ok(())
    }
} // mod imp (feature = "capture")
