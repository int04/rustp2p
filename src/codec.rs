use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, SpsPpsStrategy, UsageType,
};
use openh264::formats::{YUVBuffer, YUVSource};
use openh264::OpenH264API;

use crate::convert::bgra_to_i420;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum VideoBackend {
    Cpu,
    Gpu,
}

impl VideoBackend {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Gpu => "GPU",
        }
    }
}

pub struct VideoEncoder {
    inner: VideoEncoderInner,
}

enum VideoEncoderInner {
    Cpu(CpuVideoEncoder),
    #[cfg(target_os = "macos")]
    Gpu(VideoToolboxEncoder),
}

impl VideoEncoder {
    pub fn new(
        backend: VideoBackend,
        fps: u32,
        bitrate_mbps: u32,
        width: usize,
        height: usize,
    ) -> Result<Self> {
        let inner = match backend {
            VideoBackend::Cpu => VideoEncoderInner::Cpu(CpuVideoEncoder::new(fps, bitrate_mbps)?),
            VideoBackend::Gpu => {
                #[cfg(target_os = "macos")]
                {
                    VideoEncoderInner::Gpu(VideoToolboxEncoder::new(
                        fps,
                        bitrate_mbps,
                        width,
                        height,
                    )?)
                }
                #[cfg(not(target_os = "macos"))]
                {
                    bail!("GPU encoder is not implemented for this platform yet")
                }
            }
        };
        Ok(Self { inner })
    }

    pub fn encode_bgra(&mut self, bgra: &[u8], width: usize, height: usize) -> Result<Vec<u8>> {
        match &mut self.inner {
            VideoEncoderInner::Cpu(enc) => enc.encode_bgra(bgra, width, height),
            #[cfg(target_os = "macos")]
            VideoEncoderInner::Gpu(enc) => enc.encode_bgra(bgra, width, height),
        }
    }
}

struct CpuVideoEncoder {
    enc: Encoder,
}

impl CpuVideoEncoder {
    fn new(fps: u32, bitrate_mbps: u32) -> Result<Self> {
        let api = OpenH264API::from_source();
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate_mbps * 1_000_000))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .usage_type(UsageType::ScreenContentRealTime)
            .sps_pps_strategy(SpsPpsStrategy::IncreasingId)
            .intra_frame_period(IntraFramePeriod::from_num_frames(fps.max(1)));
        Ok(Self {
            enc: Encoder::with_api_config(api, config).context("create OpenH264 encoder")?,
        })
    }

    fn encode_bgra(&mut self, bgra: &[u8], width: usize, height: usize) -> Result<Vec<u8>> {
        let i420 = bgra_to_i420(bgra, width, height);
        let yuv = YUVBuffer::from_vec(i420, width, height);
        let stream = self.enc.encode(&yuv).context("OpenH264 encode")?;

        let mut out = Vec::new();
        for i in 0..stream.num_layers() {
            if let Some(layer) = stream.layer(i) {
                for j in 0..layer.nal_count() {
                    if let Some(nal) = layer.nal_unit(j) {
                        out.extend_from_slice(nal);
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(target_os = "macos")]
struct VideoToolboxEncoder {
    session: videotoolbox::CompressionSession,
    surface: apple_cf::iosurface::IOSurface,
    fps: u32,
    frame_index: i64,
}

#[cfg(target_os = "macos")]
impl VideoToolboxEncoder {
    fn new(fps: u32, bitrate_mbps: u32, width: usize, height: usize) -> Result<Self> {
        use videotoolbox::prelude::*;

        let width_i32 = i32::try_from(width).context("width too large for VideoToolbox")?;
        let height_i32 = i32::try_from(height).context("height too large for VideoToolbox")?;
        let surface = apple_cf::iosurface::IOSurface::create(
            width,
            height,
            u32::from_be_bytes(*b"BGRA"),
            4,
        )
        .ok_or_else(|| anyhow::anyhow!("allocate BGRA IOSurface"))?;

        let session = CompressionSession::builder(width_i32, height_i32, Codec::H264)
            .with_real_time(true)
            .with_hardware_acceleration(HardwareAcceleration::Required)
            .with_average_bit_rate(i32::try_from(bitrate_mbps.saturating_mul(1_000_000))
                .unwrap_or(i32::MAX))
            .with_expected_frame_rate(fps as f64)
            .with_max_keyframe_interval(i32::try_from(fps.max(1)).unwrap_or(i32::MAX))
            .build()
            .context("create VideoToolbox hardware H.264 encoder")?;

        Ok(Self {
            session,
            surface,
            fps: fps.max(1),
            frame_index: 0,
        })
    }

    fn encode_bgra(&mut self, bgra: &[u8], width: usize, height: usize) -> Result<Vec<u8>> {
        use apple_cf::cm::CMTime;
        use apple_cf::iosurface::IOSurfaceLockOptions;

        if width != self.surface.width() || height != self.surface.height() {
            bail!(
                "VideoToolbox surface size changed: got {width}x{height}, expected {}x{}",
                self.surface.width(),
                self.surface.height()
            );
        }
        let src_stride = width
            .checked_mul(4)
            .ok_or_else(|| anyhow::anyhow!("BGRA stride overflow"))?;
        if bgra.len() < src_stride.saturating_mul(height) {
            bail!("BGRA frame is smaller than expected");
        }

        {
            let mut guard = self
                .surface
                .lock(IOSurfaceLockOptions::NONE)
                .map_err(|code| anyhow::anyhow!("lock IOSurface failed: {code}"))?;
            let dst_stride = guard.bytes_per_row();
            let dst = unsafe { guard.as_slice_mut() }
                .ok_or_else(|| anyhow::anyhow!("IOSurface has no writable byte slice"))?;
            for y in 0..height {
                let src_off = y * src_stride;
                let dst_off = y * dst_stride;
                dst[dst_off..dst_off + src_stride]
                    .copy_from_slice(&bgra[src_off..src_off + src_stride]);
            }
        }

        let encoded = self
            .session
            .encode(&self.surface, CMTime::new(self.frame_index, self.fps as i32))
            .context("VideoToolbox H.264 encode")?;
        self.frame_index = self.frame_index.wrapping_add(1);

        if encoded.data.is_empty() {
            return Ok(Vec::new());
        }

        let sample = encoded
            .cm_sample_buffer()
            .ok_or_else(|| anyhow::anyhow!("VideoToolbox returned no sample buffer"))?;
        let format = sample
            .format_description()
            .ok_or_else(|| anyhow::anyhow!("VideoToolbox sample has no format description"))?;
        let sets = format
            .video_parameter_sets()
            .map_err(|code| anyhow::anyhow!("read H.264 parameter sets failed: {code}"))?;

        let mut out = Vec::with_capacity(encoded.data.len() + 128);
        if sample.is_sync_sample() {
            for parameter_set in &sets.parameter_sets {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(parameter_set);
            }
        }
        avcc_to_annex_b(&encoded.data, sets.nal_unit_header_length as usize, &mut out)?;
        Ok(out)
    }
}

#[cfg(target_os = "macos")]
fn avcc_to_annex_b(data: &[u8], header_len: usize, out: &mut Vec<u8>) -> Result<()> {
    if !(1..=4).contains(&header_len) {
        bail!("unsupported H.264 NAL length field: {header_len}");
    }
    let mut offset = 0usize;
    while offset + header_len <= data.len() {
        let mut nal_len = 0usize;
        for &b in &data[offset..offset + header_len] {
            nal_len = (nal_len << 8) | usize::from(b);
        }
        offset += header_len;
        if nal_len == 0 || offset + nal_len > data.len() {
            bail!("invalid AVCC H.264 access unit");
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[offset..offset + nal_len]);
        offset += nal_len;
    }
    if offset != data.len() {
        bail!("trailing bytes in AVCC H.264 access unit");
    }
    Ok(())
}

pub struct VideoDecoder {
    inner: VideoDecoderInner,
}

enum VideoDecoderInner {
    Cpu(Decoder),
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    Gpu(HardwareVideoDecoder),
}

impl VideoDecoder {
    pub fn new(backend: VideoBackend, fps: u32) -> Result<Self> {
        match backend {
            VideoBackend::Cpu => Ok(Self {
                inner: VideoDecoderInner::Cpu(
                    Decoder::new().context("create OpenH264 decoder")?,
                ),
            }),
            VideoBackend::Gpu => {
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                {
                    Ok(Self {
                        inner: VideoDecoderInner::Gpu(HardwareVideoDecoder::new(fps)?),
                    })
                }
                #[cfg(not(any(target_os = "macos", target_os = "windows")))]
                {
                    bail!("GPU decoder is currently supported only on macOS and Windows")
                }
            }
        }
    }

    pub fn decode(&mut self, nal: &[u8]) -> Result<Option<(Vec<u8>, u32, u32)>> {
        match &mut self.inner {
            VideoDecoderInner::Cpu(dec) => {
                let maybe = dec.decode(nal).context("OpenH264 decode")?;
                let Some(yuv) = maybe else { return Ok(None) };

                let (w, h) = yuv.dimensions();
                let mut rgba = vec![0u8; w * h * 4];
                yuv.write_rgba8(&mut rgba);
                Ok(Some((rgba, w as u32, h as u32)))
            }
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            VideoDecoderInner::Gpu(dec) => dec.decode(nal),
        }
    }

    pub fn backend_status(&self) -> Option<&str> {
        match &self.inner {
            VideoDecoderInner::Cpu(_) => None,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            VideoDecoderInner::Gpu(dec) => Some(&dec.last_outcome),
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
struct HardwareVideoDecoder {
    dec: openipc_video::PlatformDecoder,
    timestamp: i64,
    timestamp_step: i64,
    last_outcome: String,
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl HardwareVideoDecoder {
    fn new(fps: u32) -> Result<Self> {
        let options = openipc_video::DecoderOptions {
            max_frames_in_flight: 3,
            low_latency: true,
            require_hardware: true,
        };
        use openipc_video::{VideoCodec, VideoDecoder as _};

        let dec = openipc_video::PlatformDecoder::new(options)
            .context("create hardware H.264 decoder")?;
        let caps = dec.capabilities();
        let h264 = caps.codec(VideoCodec::H264)
            .ok_or_else(|| anyhow::anyhow!("hardware decoder reports no H.264 capability"))?;
        if !h264.supported {
            bail!("hardware H.264 decoder is not supported by the selected GPU adapter");
        }
        if !h264.hardware_accelerated {
            bail!("H.264 decode is available but not hardware accelerated on the selected GPU adapter");
        }
        let timestamp_step = (90_000_i64 / i64::from(fps.max(1))).max(1);
        Ok(Self {
            dec,
            timestamp: 0,
            timestamp_step,
            last_outcome: "initialized".to_string(),
        })
    }

    fn decode(&mut self, nal: &[u8]) -> Result<Option<(Vec<u8>, u32, u32)>> {
        use openipc_video::{EncodedAccessUnit, VideoCodec, VideoDecoder as _, VideoTimestamp};

        let timestamp = VideoTimestamp::new(self.timestamp, 90_000)
            .ok_or_else(|| anyhow::anyhow!("invalid hardware decoder timestamp"))?;
        self.timestamp = self.timestamp.wrapping_add(self.timestamp_step);
        let is_keyframe = h264_contains_idr(nal);
        let access = EncodedAccessUnit::new(
            VideoCodec::H264,
            nal.to_vec(),
            timestamp,
            is_keyframe,
        );
        let outcome = self.dec
            .submit(access)
            .context("hardware H.264 decode submit")?;
        let outcome_text = format!("{outcome:?}");
        if outcome_text != self.last_outcome {
            tracing::warn!("GPU decoder submit outcome: {outcome_text}");
            self.last_outcome = outcome_text;
        }

        let Some(frame) = self.dec.latest_frame() else {
            return Ok(None);
        };

        hardware_frame_to_rgba(&self.dec, frame.surface)
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn h264_contains_idr(data: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 4 <= data.len() {
        let (start, prefix_len) = if data[i..].starts_with(&[0, 0, 0, 1]) {
            (i, 4)
        } else if data[i..].starts_with(&[0, 0, 1]) {
            (i, 3)
        } else {
            i += 1;
            continue;
        };

        let nal_pos = start + prefix_len;
        if nal_pos < data.len() && (data[nal_pos] & 0x1f) == 5 {
            return true;
        }
        i = nal_pos.saturating_add(1);
    }
    false
}

#[cfg(target_os = "macos")]
fn hardware_frame_to_rgba(
    _decoder: &openipc_video::PlatformDecoder,
    surface: openipc_video::MacOsVideoFrame,
) -> Result<Option<(Vec<u8>, u32, u32)>> {
    use openipc_video::{DecodedSurface, PixelFormat};

    let dims = surface.dimensions();
    let width = dims.width as usize;
    let height = dims.height as usize;
    match surface.pixel_format() {
        PixelFormat::Bgra8 => {
            let rgba = surface
                .with_mapped_planes(|planes| {
                    let plane = &planes[0];
                    bgra_rows_to_rgba(plane.data(), plane.stride(), width, height)
                })
                .context("map VideoToolbox BGRA frame")?;
            Ok(Some((rgba, dims.width, dims.height)))
        }
        PixelFormat::Nv12VideoRange | PixelFormat::Nv12FullRange => {
            let full_range = matches!(surface.pixel_format(), PixelFormat::Nv12FullRange);
            let rgba = surface
                .with_mapped_planes(|planes| {
                    if planes.len() < 2 {
                        return Vec::new();
                    }
                    nv12_to_rgba(
                        planes[0].data(),
                        planes[0].stride(),
                        planes[1].data(),
                        planes[1].stride(),
                        width,
                        height,
                        full_range,
                    )
                })
                .context("map VideoToolbox NV12 frame")?;
            if rgba.is_empty() {
                bail!("VideoToolbox returned an invalid NV12 frame");
            }
            Ok(Some((rgba, dims.width, dims.height)))
        }
        other => bail!("unsupported VideoToolbox pixel format: {other:?}"),
    }
}

#[cfg(target_os = "windows")]
fn hardware_frame_to_rgba(
    decoder: &openipc_video::PlatformDecoder,
    surface: openipc_video::WindowsVideoFrame,
) -> Result<Option<(Vec<u8>, u32, u32)>> {
    let frame = decoder
        .copy_nv12(&surface)
        .context("read back Media Foundation NV12 frame")?;
    let dims = frame.dimensions();
    let rgba = nv12_to_rgba(
        frame.y_plane(),
        frame.stride(),
        frame.uv_plane(),
        frame.stride(),
        dims.width as usize,
        dims.height as usize,
        false,
    );
    Ok(Some((rgba, dims.width, dims.height)))
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn bgra_rows_to_rgba(
    bgra: &[u8],
    stride: usize,
    width: usize,
    height: usize,
) -> Vec<u8> {
    let mut rgba = vec![0_u8; width.saturating_mul(height).saturating_mul(4)];
    for y in 0..height {
        let src = &bgra[y * stride..y * stride + width * 4];
        let dst = &mut rgba[y * width * 4..(y + 1) * width * 4];
        for (src_px, dst_px) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
            dst_px[0] = src_px[2];
            dst_px[1] = src_px[1];
            dst_px[2] = src_px[0];
            dst_px[3] = src_px[3];
        }
    }
    rgba
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn nv12_to_rgba(
    y_plane: &[u8],
    y_stride: usize,
    uv_plane: &[u8],
    uv_stride: usize,
    width: usize,
    height: usize,
    full_range: bool,
) -> Vec<u8> {
    let mut rgba = vec![0_u8; width.saturating_mul(height).saturating_mul(4)];
    for y in 0..height {
        for x in 0..width {
            let yy = i32::from(y_plane[y * y_stride + x]);
            let uv = (y / 2) * uv_stride + (x / 2) * 2;
            let u = i32::from(uv_plane[uv]) - 128;
            let v = i32::from(uv_plane[uv + 1]) - 128;

            let (r, g, b) = if full_range {
                (
                    yy + ((359 * v) >> 8),
                    yy - ((88 * u + 183 * v) >> 8),
                    yy + ((454 * u) >> 8),
                )
            } else {
                let c = (yy - 16).max(0);
                (
                    (298 * c + 409 * v + 128) >> 8,
                    (298 * c - 100 * u - 208 * v + 128) >> 8,
                    (298 * c + 516 * u + 128) >> 8,
                )
            };
            let o = (y * width + x) * 4;
            rgba[o] = r.clamp(0, 255) as u8;
            rgba[o + 1] = g.clamp(0, 255) as u8;
            rgba[o + 2] = b.clamp(0, 255) as u8;
            rgba[o + 3] = 255;
        }
    }
    rgba
}
