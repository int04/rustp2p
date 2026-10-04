use serde::{Deserialize, Serialize};

/// TCP control port — bidirectional messages
pub const CONTROL_PORT: u16 = 7272;
/// UDP video port — host streams to viewer on this port
pub const VIDEO_PORT: u16 = 7274;
/// Max H.264 payload bytes per UDP datagram (stays under 1400-byte Ethernet MTU)
pub const VIDEO_CHUNK_MAX: usize = 1300;
/// UDP header layout: frame_id(4) + chunk_idx(2) + total_chunks(2) + pts_ms(4) + data_len(2)
pub const VIDEO_HDR_LEN: usize = 14;

/// Messages sent over the TCP control channel
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlMsg {
    /// Viewer → Host: initiate session. `udp_port` is the UDP port the viewer is
    /// listening on for the video stream (chosen per-connection so one viewer can
    /// receive from multiple hosts without port collisions).
    Hello {
        udp_port: u16,
    },
    /// Host → Viewer: screen dimensions and stream settings
    Welcome {
        width: u32,
        height: u32,
        fps: u32,
    },

    Ping,
    Pong,
}

/// One UDP datagram carrying a slice of an encoded video frame
pub struct VideoPacket<'a> {
    pub frame_id: u32,
    pub chunk_idx: u16,
    pub total_chunks: u16,
    pub pts_ms: u32,
    pub data: &'a [u8],
}

impl<'a> VideoPacket<'a> {
    pub fn write_to(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.frame_id.to_be_bytes());
        buf.extend_from_slice(&self.chunk_idx.to_be_bytes());
        buf.extend_from_slice(&self.total_chunks.to_be_bytes());
        buf.extend_from_slice(&self.pts_ms.to_be_bytes());
        buf.extend_from_slice(&(self.data.len() as u16).to_be_bytes());
        buf.extend_from_slice(self.data);
    }
}

/// Owned parsed UDP datagram
pub struct InboundVideo {
    pub frame_id: u32,
    pub chunk_idx: u16,
    pub total_chunks: u16,
    pub pts_ms: u32,
    pub data: Vec<u8>,
}

impl InboundVideo {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < VIDEO_HDR_LEN {
            return None;
        }
        let frame_id = u32::from_be_bytes(buf[0..4].try_into().ok()?);
        let chunk_idx = u16::from_be_bytes(buf[4..6].try_into().ok()?);
        let total_chunks = u16::from_be_bytes(buf[6..8].try_into().ok()?);
        let pts_ms = u32::from_be_bytes(buf[8..12].try_into().ok()?);
        let data_len = u16::from_be_bytes(buf[12..14].try_into().ok()?) as usize;
        if buf.len() < VIDEO_HDR_LEN + data_len {
            return None;
        }
        Some(Self {
            frame_id,
            chunk_idx,
            total_chunks,
            pts_ms,
            data: buf[VIDEO_HDR_LEN..VIDEO_HDR_LEN + data_len].to_vec(),
        })
    }
}
