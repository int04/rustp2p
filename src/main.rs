mod codec;
mod convert;
mod crypto;
mod gui;
mod host;
mod proto;
mod transport;
mod viewer;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::codec::VideoBackend;

#[derive(Parser)]
#[command(
    name = "rust-p2p-viewer",
    about = "Direct LAN peer-to-peer screen viewer — low latency"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Share this machine's screen for view-only clients
    Host {
        #[arg(short, long, default_value = "0.0.0.0", help = "Bind address")]
        bind: String,
        #[arg(short, long, default_value = "7272", help = "TCP control port")]
        port: u16,
        #[arg(long, default_value = "60", help = "Target capture FPS")]
        fps: u32,
        #[arg(long, default_value = "8", help = "H.264 bitrate in Mbps")]
        bitrate: u32,
        #[arg(long, value_enum, default_value_t = VideoBackend::Cpu, help = "Encoder backend: cpu (OpenH264) or gpu (hardware where supported)")]
        encoder: VideoBackend,
        #[arg(short = 'k', long, default_value = "", help = "Connection password")]
        password: String,
    },
    /// Connect and view a remote host (view-only)
    View {
        #[arg(help = "Host IP address or hostname")]
        host: String,
        #[arg(short, long, default_value = "7272", help = "Host TCP control port")]
        port: u16,
        #[arg(short = 'k', long, default_value = "", help = "Connection password")]
        password: String,
        #[arg(long, value_enum, default_value_t = VideoBackend::Cpu, help = "Decoder backend: cpu (OpenH264) or gpu (hardware where supported)")]
        decoder: VideoBackend,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("rust_p2p_viewer=info".parse()?),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        None => gui::run(),
        Some(Cmd::Host {
            bind,
            port,
            fps,
            bitrate,
            encoder,
            password,
        }) => host::run(&bind, port, fps, bitrate, encoder, &password),
        Some(Cmd::View {
            host,
            port,
            password,
            decoder,
        }) => viewer::run(&host, port, &password, decoder),
    }
}
