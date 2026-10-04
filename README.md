# rust-p2p-viewer

Direct LAN screen viewer — no relay server, no cloud, maximum performance.  
Strictly view-only: the viewer receives video but cannot inject mouse or keyboard input.

## Architecture

```
Host (screen source)                  Viewer (view-only client)
├─ scap: DXGI / ScreenCaptureKit       ├─ UDP receive + reassemble
├─ H.264 encode: CPU/OpenH264          ├─ H.264 decode: CPU/OpenH264
│  or GPU/VideoToolbox (macOS)         │  or GPU/VideoToolbox/Media Foundation
├─ UDP → viewer (video)                ├─ egui/wgpu render
└─ TCP :7272 handshake/session         └─ TCP → host:7272 handshake/session
```

- **Video**: UDP, chunked H.264 NAL units, host→viewer
- **Control channel**: TCP is limited to handshake/session liveness; no remote input, clipboard, or file transfer
- **Latency target**: < 16 ms end-to-end on gigabit LAN

## Requirements

| Platform | Dependency |
|----------|-----------|
| macOS (host) | Screen Recording permission in System Settings → Privacy & Security |
| Windows (host) | No extra permissions needed for DXGI capture |
| Both | Rust 1.78+ |

OpenH264 downloads Cisco's prebuilt library at build time — internet required for first build. On macOS, GPU encoding uses Apple VideoToolbox. GPU decoding uses VideoToolbox on macOS and Media Foundation + D3D11 on Windows.

## Build

```bash
cargo build --release
```

## Usage

**On the machine sharing its screen (host):**
```bash
./rust-p2p-viewer host
# or with options:
./rust-p2p-viewer host --fps 120 --bitrate 12 --encoder gpu
```

**On the viewing machine (viewer):**
```bash
./rust-p2p-viewer view 192.168.1.X --decoder gpu
```

The viewer window is strictly view-only. Mouse and keyboard events are never forwarded to the host.

GUI FPS presets: **30, 60, 70, 90, 120, 150, 240 FPS**. Both HOST and CONNECT tabs expose a **CPU / GPU** backend selector.

- **Host CPU**: OpenH264 software encoder.
- **Host GPU (macOS)**: Apple VideoToolbox hardware H.264 encoder. GPU host encoding is not implemented on Windows yet.
- **Client CPU**: OpenH264 software decoder.
- **Client GPU**: VideoToolbox hardware decode on macOS; Media Foundation + D3D11 hardware decode on Windows.

## Options

```
rust-p2p-viewer host [OPTIONS]
  -b, --bind <IP>      Bind address [default: 0.0.0.0]
  -p, --port <PORT>    TCP session port [default: 7272]
      --fps <N>        Capture FPS [default: 60]
      --bitrate <N>    H.264 bitrate in Mbps [default: 8]
      --encoder <cpu|gpu> Encoder backend [default: cpu]

rust-p2p-viewer view <HOST> [OPTIONS]
  -p, --port <PORT>    Host TCP session port [default: 7272]
      --decoder <cpu|gpu> Decoder backend [default: cpu]
```

## Troubleshooting

**macOS: "Screen Recording permission required"**  
Go to System Settings → Privacy & Security → Screen Recording → add the terminal app.

**Blank window / no video**  
Check that UDP port 7274 is not blocked by a firewall on either machine.

**Compilation errors in codec.rs**  
The `openh264` Rust crate API changed across versions. If `EncoderConfig::new()` fails,  
try `EncoderConfig::new(width as u32, height as u32)`. If `dimension_rgb()` or `strides_yuv()`  
don't exist, check the crate docs for the equivalent dimension/stride accessors on `DecodedYUV`.
