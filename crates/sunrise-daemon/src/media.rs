use std::{env, fs, time::Duration};

use anyhow::{Context, Result};
use tracing::{info, warn};

const RTP_VIDEO_PAYLOAD_TYPE: u8 = 96;
const RTP_AUDIO_PAYLOAD_TYPE: u8 = 97;
const RTP_VIDEO_HEADER_LEN: usize = 32;
const NV_VIDEO_HEADER_LEN: usize = 16;
const RTP_AUDIO_HEADER_LEN: usize = 12;
const VIDEO_PACKET_SIZE: usize = 1024;
const VIDEO_MAGIC: &[u8; 8] = b"\x017charss";
const VIDEO_SSRC: u32 = 0x5253_5650;
const AUDIO_SSRC: u32 = 0x5253_4150;
const VIDEO_CLOCK_RATE: u32 = 90_000;

pub(crate) struct EncodedVideoFrame {
    pub(crate) data: Vec<u8>,
    pub(crate) frame_index: u32,
    pub(crate) timestamp_90khz: u32,
}

pub(crate) trait VideoSource: Send {
    fn description(&self) -> &str;
    fn frame_count_hint(&self) -> Option<usize>;
    fn frame_interval(&self) -> Duration;
    fn next_frame(&mut self) -> Result<EncodedVideoFrame>;
    fn request_idr(&mut self) -> Result<()> {
        Ok(())
    }
}

pub(crate) struct AnnexBVideoSource {
    description: String,
    frames: Vec<Vec<u8>>,
    next_index: usize,
    frame_index: u32,
    frame_interval: Duration,
    timestamp_step: u32,
}

impl AnnexBVideoSource {
    pub(crate) fn from_env() -> Self {
        match env::var("SUNRISE_H264_PATH") {
            Ok(path) => Self::from_path(path),
            Err(_) => {
                warn!("SUNRISE_H264_PATH is not set; using a tiny non-decodable H.264 placeholder");
                Self::from_frames(
                    "fallback H.264 placeholder".to_string(),
                    vec![fallback_h264_frame()],
                    30,
                )
            }
        }
    }

    fn from_path(path: String) -> Self {
        match fs::read(&path) {
            Ok(data) => {
                let frames = split_annex_b_access_units(&data);
                if frames.is_empty() {
                    warn!(
                        %path,
                        "H.264 source contained no Annex B NAL units; using raw file as one frame"
                    );
                    Self::from_frames(format!("raw H.264 file {path}"), vec![data], 30)
                } else {
                    info!(%path, frames = frames.len(), "loaded H.264 video source");
                    Self::from_frames(format!("Annex B H.264 file {path}"), frames, 30)
                }
            }
            Err(err) => {
                warn!(%path, %err, "failed to read H.264 source; using placeholder");
                Self::from_frames(
                    "fallback H.264 placeholder".to_string(),
                    vec![fallback_h264_frame()],
                    30,
                )
            }
        }
    }

    fn from_frames(description: String, frames: Vec<Vec<u8>>, fps: u32) -> Self {
        let fps = fps.max(1);
        Self {
            description,
            frames,
            next_index: 0,
            frame_index: 1,
            frame_interval: Duration::from_millis(u64::from(1000 / fps)),
            timestamp_step: VIDEO_CLOCK_RATE / fps,
        }
    }
}

impl VideoSource for AnnexBVideoSource {
    fn description(&self) -> &str {
        &self.description
    }

    fn frame_count_hint(&self) -> Option<usize> {
        Some(self.frames.len())
    }

    fn frame_interval(&self) -> Duration {
        self.frame_interval
    }

    fn next_frame(&mut self) -> Result<EncodedVideoFrame> {
        let frame = self
            .frames
            .get(self.next_index)
            .context("video source contained no frames")?
            .clone();
        let frame_index = self.frame_index;
        let timestamp_90khz = frame_index.saturating_mul(self.timestamp_step);

        self.next_index = (self.next_index + 1) % self.frames.len();
        self.frame_index = self.frame_index.wrapping_add(1);

        Ok(EncodedVideoFrame {
            data: frame,
            frame_index,
            timestamp_90khz,
        })
    }

    fn request_idr(&mut self) -> Result<()> {
        self.next_index = 0;
        Ok(())
    }
}

pub(crate) struct VideoPacketizer {
    stream_packet_index: u32,
    sequence: u16,
}

impl VideoPacketizer {
    pub(crate) fn new() -> Self {
        Self {
            stream_packet_index: 0,
            sequence: 1,
        }
    }

    pub(crate) fn packetize(&mut self, frame: &EncodedVideoFrame) -> Vec<Vec<u8>> {
        build_video_rtp_packets(
            &frame.data,
            frame.frame_index,
            &mut self.stream_packet_index,
            &mut self.sequence,
            frame.timestamp_90khz,
        )
    }
}

pub(crate) struct EncodedAudioPacket {
    pub(crate) payload: Vec<u8>,
    pub(crate) timestamp: u32,
}

pub(crate) struct OpusSilenceSource {
    timestamp: u32,
}

impl OpusSilenceSource {
    pub(crate) fn new() -> Self {
        Self { timestamp: 0 }
    }

    pub(crate) fn packet_interval(&self) -> Duration {
        Duration::from_millis(20)
    }

    pub(crate) fn next_packet(&mut self) -> EncodedAudioPacket {
        let packet = EncodedAudioPacket {
            payload: vec![0xF8, 0xFF, 0xFE],
            timestamp: self.timestamp,
        };
        self.timestamp = self.timestamp.wrapping_add(960);
        packet
    }
}

pub(crate) struct AudioPacketizer {
    sequence: u16,
}

impl AudioPacketizer {
    pub(crate) fn new() -> Self {
        Self { sequence: 1 }
    }

    pub(crate) fn packetize(&mut self, packet: &EncodedAudioPacket) -> Vec<u8> {
        let rtp = build_audio_rtp_packet(self.sequence, packet.timestamp, &packet.payload);
        self.sequence = self.sequence.wrapping_add(1);
        rtp
    }
}

pub(crate) fn split_annex_b_access_units(data: &[u8]) -> Vec<Vec<u8>> {
    let units = split_annex_b_units(data);
    let mut frames = Vec::new();
    let mut current = Vec::new();
    let mut current_has_picture = false;

    for unit in units {
        let unit_type = nal_unit_type(&unit);
        let slice_first_mb = match unit_type {
            Some(1 | 5) => h264_first_mb_in_slice(&unit),
            _ => None,
        };
        let starts_picture = unit_type.is_some_and(|unit_type| unit_type == 1 || unit_type == 5);
        let starts_new_picture =
            starts_picture && slice_first_mb.map_or(current_has_picture, |first_mb| first_mb == 0);
        let starts_next_header = current_has_picture && matches!(unit_type, Some(6 | 7 | 8 | 9));

        if starts_next_header || (starts_new_picture && current_has_picture) {
            frames.push(std::mem::take(&mut current));
            current_has_picture = false;
        }

        current.extend_from_slice(&unit);
        current_has_picture |= starts_picture;
    }

    if !current.is_empty() {
        frames.push(current);
    }

    frames
}

fn split_annex_b_units(data: &[u8]) -> Vec<Vec<u8>> {
    let starts = annex_b_start_codes(data);
    if starts.is_empty() {
        return Vec::new();
    }

    starts
        .iter()
        .enumerate()
        .map(|(index, &start)| {
            let end = starts.get(index + 1).copied().unwrap_or(data.len());
            data[start..end].to_vec()
        })
        .collect()
}

fn annex_b_start_codes(data: &[u8]) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        if data[index..].starts_with(&[0, 0, 1]) {
            starts.push(index);
            index += 3;
        } else if data[index..].starts_with(&[0, 0, 0, 1]) {
            starts.push(index);
            index += 4;
        } else {
            index += 1;
        }
    }
    starts
}

fn nal_unit_type(unit: &[u8]) -> Option<u8> {
    let header = if unit.starts_with(&[0, 0, 0, 1]) {
        unit.get(4)
    } else if unit.starts_with(&[0, 0, 1]) {
        unit.get(3)
    } else {
        None
    }?;
    Some(header & 0x1F)
}

fn h264_first_mb_in_slice(unit: &[u8]) -> Option<u32> {
    let header_offset = nal_header_offset(unit)?;
    let rbsp = rbsp_without_emulation_prevention(&unit[header_offset + 1..]);
    read_unsigned_exp_golomb(&rbsp)
}

fn nal_header_offset(unit: &[u8]) -> Option<usize> {
    if unit.starts_with(&[0, 0, 0, 1]) {
        Some(4)
    } else if unit.starts_with(&[0, 0, 1]) {
        Some(3)
    } else {
        None
    }
}

fn rbsp_without_emulation_prevention(payload: &[u8]) -> Vec<u8> {
    let mut rbsp = Vec::with_capacity(payload.len());
    let mut zero_run = 0_u8;

    for &byte in payload {
        if zero_run >= 2 && byte == 0x03 {
            zero_run = 0;
            continue;
        }

        rbsp.push(byte);
        zero_run = if byte == 0 {
            zero_run.saturating_add(1)
        } else {
            0
        };
    }

    rbsp
}

fn read_unsigned_exp_golomb(data: &[u8]) -> Option<u32> {
    let mut reader = BitReader::new(data);
    let mut leading_zero_bits = 0_u32;

    while !reader.read_bit()? {
        leading_zero_bits += 1;
        if leading_zero_bits >= 32 {
            return None;
        }
    }

    let suffix = reader.read_bits(leading_zero_bits)?;
    Some((1_u32 << leading_zero_bits) - 1 + suffix)
}

struct BitReader<'a> {
    data: &'a [u8],
    bit_index: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_index: 0 }
    }

    fn read_bit(&mut self) -> Option<bool> {
        let byte = *self.data.get(self.bit_index / 8)?;
        let bit = (byte >> (7 - (self.bit_index % 8))) & 1;
        self.bit_index += 1;
        Some(bit != 0)
    }

    fn read_bits(&mut self, count: u32) -> Option<u32> {
        let mut value = 0_u32;
        for _ in 0..count {
            value = (value << 1) | u32::from(self.read_bit()?);
        }
        Some(value)
    }
}

fn fallback_h264_frame() -> Vec<u8> {
    vec![
        0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1f, 0x8d, 0x68, 0x50, 0x1e, 0xd0, 0x0f, 0x12,
        0x26, 0xa0,
    ]
}

fn build_video_rtp_packets(
    frame: &[u8],
    frame_index: u32,
    stream_packet_index: &mut u32,
    sequence: &mut u16,
    timestamp: u32,
) -> Vec<Vec<u8>> {
    let first_payload_size = VIDEO_PACKET_SIZE - NV_VIDEO_HEADER_LEN - VIDEO_MAGIC.len();
    let regular_payload_size = VIDEO_PACKET_SIZE - NV_VIDEO_HEADER_LEN;
    let data_packet_count =
        video_data_packet_count(frame.len(), first_payload_size, regular_payload_size);
    let mut chunks = Vec::new();
    let mut offset = 0;
    let mut first = true;
    let mut packet_in_frame = 0_u16;

    while offset < frame.len() || first {
        let payload_size = if first {
            first_payload_size
        } else {
            regular_payload_size
        };
        let end = (offset + payload_size).min(frame.len());
        let payload = &frame[offset..end];
        let last = end >= frame.len();

        let mut packet =
            Vec::with_capacity(RTP_VIDEO_HEADER_LEN + VIDEO_MAGIC.len() + payload.len());
        append_rtp_header(
            &mut packet,
            RTP_VIDEO_PAYLOAD_TYPE,
            *sequence,
            timestamp,
            VIDEO_SSRC,
            true,
        );
        append_nv_video_header(
            &mut packet,
            *stream_packet_index,
            frame_index,
            packet_in_frame,
            data_packet_count,
            video_flags(first, last),
        );
        if first {
            packet.extend_from_slice(VIDEO_MAGIC);
        }
        packet.extend_from_slice(payload);
        chunks.push(packet);

        *sequence = sequence.wrapping_add(1);
        *stream_packet_index = stream_packet_index.wrapping_add(1);
        packet_in_frame = packet_in_frame.wrapping_add(1);
        offset = end;
        first = false;
    }

    chunks
}

fn video_data_packet_count(
    frame_len: usize,
    first_payload_size: usize,
    regular_payload_size: usize,
) -> u16 {
    if frame_len <= first_payload_size {
        return 1;
    }
    let remaining = frame_len - first_payload_size;
    (1 + remaining.div_ceil(regular_payload_size)) as u16
}

fn build_audio_rtp_packet(sequence: u16, timestamp: u32, payload: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(RTP_AUDIO_HEADER_LEN + payload.len());
    append_rtp_header(
        &mut packet,
        RTP_AUDIO_PAYLOAD_TYPE,
        sequence,
        timestamp,
        AUDIO_SSRC,
        false,
    );
    packet.extend_from_slice(payload);
    packet
}

fn append_rtp_header(
    packet: &mut Vec<u8>,
    payload_type: u8,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    extension: bool,
) {
    packet.push(if extension { 0x90 } else { 0x80 });
    packet.push(payload_type);
    packet.extend_from_slice(&sequence.to_be_bytes());
    packet.extend_from_slice(&timestamp.to_be_bytes());
    packet.extend_from_slice(&ssrc.to_be_bytes());
    if extension {
        packet.extend_from_slice(&0_u16.to_be_bytes());
        packet.extend_from_slice(&0_u16.to_be_bytes());
    }
}

fn append_nv_video_header(
    packet: &mut Vec<u8>,
    stream_packet_index: u32,
    frame_index: u32,
    packet_in_frame: u16,
    data_packet_count: u16,
    flags: u8,
) {
    packet.extend_from_slice(&stream_packet_index.wrapping_shl(8).to_le_bytes());
    packet.extend_from_slice(&frame_index.to_le_bytes());
    packet.push(flags);
    packet.push(0);
    packet.push(0x10);
    packet.push(0);
    packet.extend_from_slice(&video_fec_info(packet_in_frame, data_packet_count).to_le_bytes());
}

fn video_fec_info(packet_in_frame: u16, data_packet_count: u16) -> u32 {
    (u32::from(data_packet_count) << 22) | (u32::from(packet_in_frame) << 12)
}

fn video_flags(first: bool, last: bool) -> u8 {
    let mut flags = 0x01;
    if last {
        flags |= 0x02;
    }
    if first {
        flags |= 0x04;
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annex_b_h264_into_nal_units() {
        let units = split_annex_b_units(&[
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 4, 0, 0, 0, 1, 0x65, 5, 6,
        ]);

        assert_eq!(units.len(), 3);
        assert_eq!(units[0], vec![0, 0, 0, 1, 0x67, 1, 2]);
        assert_eq!(units[1], vec![0, 0, 1, 0x68, 3, 4]);
        assert_eq!(units[2], vec![0, 0, 0, 1, 0x65, 5, 6]);
    }

    #[test]
    fn groups_h264_headers_with_following_picture() {
        let frames = split_annex_b_access_units(&[
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 4, 0, 0, 0, 1, 0x65, 0x80, 6, 0, 0, 1, 0x41,
            0x80, 8,
        ]);

        assert_eq!(frames.len(), 2);
        assert!(frames[0].starts_with(&[0, 0, 0, 1, 0x67]));
        assert!(frames[0].ends_with(&[0, 0, 0, 1, 0x65, 0x80, 6]));
        assert_eq!(frames[1], vec![0, 0, 1, 0x41, 0x80, 8]);
    }

    #[test]
    fn keeps_multiple_h264_slices_in_one_access_unit() {
        let frames = split_annex_b_access_units(&[
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 4, 0, 0, 1, 0x65, 0x80, 0, 0, 1, 0x65, 0x40,
            0, 0, 1, 0x65, 0x60, 0, 0, 1, 0x41, 0x80, 0, 0, 1, 0x41, 0x40,
        ]);

        assert_eq!(frames.len(), 2);
        assert_eq!(split_annex_b_units(&frames[0]).len(), 5);
        assert_eq!(split_annex_b_units(&frames[1]).len(), 2);
    }

    #[test]
    fn annex_b_video_source_cycles_frames_with_timestamps() {
        let mut source =
            AnnexBVideoSource::from_frames("test".to_string(), vec![vec![1], vec![2]], 30);

        let first = source.next_frame().unwrap();
        let second = source.next_frame().unwrap();
        let third = source.next_frame().unwrap();

        assert_eq!(first.data, vec![1]);
        assert_eq!(first.frame_index, 1);
        assert_eq!(first.timestamp_90khz, 3000);
        assert_eq!(second.data, vec![2]);
        assert_eq!(second.frame_index, 2);
        assert_eq!(second.timestamp_90khz, 6000);
        assert_eq!(third.data, vec![1]);
        assert_eq!(third.frame_index, 3);
    }

    #[test]
    fn annex_b_video_source_rewinds_to_first_frame_on_idr_request() {
        let mut source =
            AnnexBVideoSource::from_frames("test".to_string(), vec![vec![1], vec![2]], 30);
        assert_eq!(source.next_frame().unwrap().data, vec![1]);
        assert_eq!(source.next_frame().unwrap().data, vec![2]);

        source.request_idr().unwrap();

        assert_eq!(source.next_frame().unwrap().data, vec![1]);
    }

    #[test]
    fn builds_video_rtp_packet_with_nv_header_and_magic() {
        let mut packetizer = VideoPacketizer::new();
        let packets = packetizer.packetize(&EncodedVideoFrame {
            data: vec![0, 0, 0, 1, 0x65, 1, 2, 3],
            frame_index: 7,
            timestamp_90khz: 1234,
        });

        assert_eq!(packets.len(), 1);
        assert_eq!(&packets[0][..2], &[0x90, RTP_VIDEO_PAYLOAD_TYPE]);
        assert_eq!(&packets[0][12..16], &[0, 0, 0, 0]);
        assert_eq!(&packets[0][16..20], &(0_u32 << 8).to_le_bytes());
        assert_eq!(&packets[0][20..24], &7_u32.to_le_bytes());
        assert_eq!(packets[0][24], 0x01 | 0x02 | 0x04);
        assert_eq!(packets[0][26], 0x10);
        assert_eq!(&packets[0][28..32], &video_fec_info(0, 1).to_le_bytes());
        assert_eq!(
            &packets[0][RTP_VIDEO_HEADER_LEN..RTP_VIDEO_HEADER_LEN + 8],
            VIDEO_MAGIC
        );
    }

    #[test]
    fn builds_multi_packet_video_fec_info_and_stream_indices() {
        let mut packet_index = 2;
        let mut sequence = 1;
        let packets =
            build_video_rtp_packets(&vec![0x65; 3000], 9, &mut packet_index, &mut sequence, 3000);

        assert_eq!(packets.len(), 3);
        assert_eq!(&packets[0][16..20], &(2_u32 << 8).to_le_bytes());
        assert_eq!(&packets[1][16..20], &(3_u32 << 8).to_le_bytes());
        assert_eq!(&packets[2][16..20], &(4_u32 << 8).to_le_bytes());
        assert_eq!(&packets[0][28..32], &video_fec_info(0, 3).to_le_bytes());
        assert_eq!(&packets[1][28..32], &video_fec_info(1, 3).to_le_bytes());
        assert_eq!(&packets[2][28..32], &video_fec_info(2, 3).to_le_bytes());
    }

    #[test]
    fn builds_audio_rtp_packet_with_opus_payload_type() {
        let mut source = OpusSilenceSource::new();
        let mut packetizer = AudioPacketizer::new();
        let packet = packetizer.packetize(&source.next_packet());

        assert_eq!(&packet[..2], &[0x80, RTP_AUDIO_PAYLOAD_TYPE]);
        assert_eq!(&packet[2..4], &1_u16.to_be_bytes());
        assert_eq!(&packet[4..8], &0_u32.to_be_bytes());
        assert_eq!(&packet[RTP_AUDIO_HEADER_LEN..], &[0xF8, 0xFF, 0xFE]);
    }
}
