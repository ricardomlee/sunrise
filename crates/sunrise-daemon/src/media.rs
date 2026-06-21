use std::{env, fs, time::Duration};

use aes::{
    Aes128,
    cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray},
};
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
pub(crate) const AUDIO_SAMPLE_RATE: u32 = 48_000;
pub(crate) const AUDIO_CHANNELS: u16 = 2;
pub(crate) const AUDIO_FRAME_SAMPLES_PER_CHANNEL: usize = 960;
#[cfg(feature = "audio-windows")]
const AUDIO_MAX_OPUS_PACKET_BYTES: usize = 1275;
#[cfg(all(target_os = "windows", feature = "audio-windows"))]
const AUDIO_CAPTURE_WAIT_TIMEOUT: Duration = Duration::from_millis(100);

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

pub(crate) struct AudioSmokeOptions {
    pub(crate) packet_count: u32,
}

pub(crate) struct AudioSmokeReport {
    pub(crate) packets: u32,
    pub(crate) bytes: usize,
    pub(crate) elapsed: Duration,
    pub(crate) source_description: String,
}

#[cfg(all(target_os = "windows", feature = "audio-windows"))]
pub(crate) fn run_audio_smoke(options: AudioSmokeOptions) -> Result<AudioSmokeReport> {
    let mut source = CapturedOpusSource::new(WasapiLoopbackCapture::new()?, LibOpusEncoder::new()?);
    let source_description = source.description().to_string();
    let started = std::time::Instant::now();
    let mut bytes = 0_usize;
    for _ in 0..options.packet_count {
        bytes += source.next_packet()?.payload.len();
    }

    Ok(AudioSmokeReport {
        packets: options.packet_count,
        bytes,
        elapsed: started.elapsed(),
        source_description,
    })
}

#[cfg(not(all(target_os = "windows", feature = "audio-windows")))]
pub(crate) fn run_audio_smoke(_options: AudioSmokeOptions) -> Result<AudioSmokeReport> {
    anyhow::bail!(
        "audio-smoke requires Windows and the audio-windows feature; run: cargo run -p sunrise-daemon --features audio-windows -- audio-smoke"
    )
}

pub(crate) trait AudioSource: Send {
    fn description(&self) -> &str;
    fn packet_interval(&self) -> Duration;
    fn next_packet(&mut self) -> Result<EncodedAudioPacket>;
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PcmAudioFrame {
    pub(crate) samples: Vec<f32>,
    pub(crate) sample_rate: u32,
    pub(crate) channels: u16,
}

pub(crate) trait PcmAudioCapture: Send {
    fn description(&self) -> &str;
    fn next_frame(&mut self) -> Result<PcmAudioFrame>;
}

pub(crate) trait OpusFrameEncoder: Send {
    fn encode_float(&mut self, samples: &[f32]) -> Result<Vec<u8>>;
}

pub(crate) struct OpusSilenceSource {
    timestamp: u32,
}

impl OpusSilenceSource {
    pub(crate) fn new() -> Self {
        Self { timestamp: 0 }
    }
}

impl AudioSource for OpusSilenceSource {
    fn description(&self) -> &str {
        "synthetic Opus silence"
    }

    fn packet_interval(&self) -> Duration {
        Duration::from_millis(20)
    }

    fn next_packet(&mut self) -> Result<EncodedAudioPacket> {
        let packet = EncodedAudioPacket {
            payload: vec![0xF8, 0xFF, 0xFE],
            timestamp: self.timestamp,
        };
        self.timestamp = self.timestamp.wrapping_add(960);
        Ok(packet)
    }
}

pub(crate) struct CapturedOpusSource<C, E> {
    capture: C,
    encoder: E,
    timestamp: u32,
    description: String,
}

impl<C, E> CapturedOpusSource<C, E>
where
    C: PcmAudioCapture,
    E: OpusFrameEncoder,
{
    pub(crate) fn new(capture: C, encoder: E) -> Self {
        let description = format!("{} via Opus", capture.description());
        Self {
            capture,
            encoder,
            timestamp: 0,
            description,
        }
    }
}

impl<C, E> AudioSource for CapturedOpusSource<C, E>
where
    C: PcmAudioCapture,
    E: OpusFrameEncoder,
{
    fn description(&self) -> &str {
        &self.description
    }

    fn packet_interval(&self) -> Duration {
        Duration::from_millis(20)
    }

    fn next_packet(&mut self) -> Result<EncodedAudioPacket> {
        let frame = self.capture.next_frame()?;
        validate_opus_frame(&frame)?;
        let payload = self.encoder.encode_float(&frame.samples)?;
        let packet = EncodedAudioPacket {
            payload,
            timestamp: self.timestamp,
        };
        self.timestamp = self.timestamp.wrapping_add(960);
        Ok(packet)
    }
}

fn validate_opus_frame(frame: &PcmAudioFrame) -> Result<()> {
    if frame.sample_rate != AUDIO_SAMPLE_RATE {
        anyhow::bail!(
            "captured audio sample rate {} did not match required Opus RTP rate {}",
            frame.sample_rate,
            AUDIO_SAMPLE_RATE
        );
    }
    if frame.channels != AUDIO_CHANNELS {
        anyhow::bail!(
            "captured audio channel count {} did not match required stereo channel count {}",
            frame.channels,
            AUDIO_CHANNELS
        );
    }
    let expected_samples = AUDIO_FRAME_SAMPLES_PER_CHANNEL * usize::from(AUDIO_CHANNELS);
    if frame.samples.len() != expected_samples {
        anyhow::bail!(
            "captured audio frame had {} samples; expected {expected_samples}",
            frame.samples.len()
        );
    }
    Ok(())
}

#[cfg(feature = "audio-windows")]
pub(crate) struct LibOpusEncoder {
    encoder: *mut libopus_sys::OpusEncoder,
}

#[cfg(feature = "audio-windows")]
impl LibOpusEncoder {
    pub(crate) fn new() -> Result<Self> {
        let mut error = 0_i32;
        let encoder = unsafe {
            libopus_sys::opus_encoder_create(
                AUDIO_SAMPLE_RATE as i32,
                i32::from(AUDIO_CHANNELS),
                libopus_sys::OPUS_APPLICATION_AUDIO as i32,
                &mut error,
            )
        };
        if encoder.is_null() || error != libopus_sys::OPUS_OK as i32 {
            anyhow::bail!("failed to create Opus encoder: error code {error}");
        }
        Ok(Self { encoder })
    }
}

#[cfg(feature = "audio-windows")]
impl Drop for LibOpusEncoder {
    fn drop(&mut self) {
        unsafe {
            libopus_sys::opus_encoder_destroy(self.encoder);
        }
    }
}

#[cfg(feature = "audio-windows")]
unsafe impl Send for LibOpusEncoder {}

#[cfg(feature = "audio-windows")]
impl OpusFrameEncoder for LibOpusEncoder {
    fn encode_float(&mut self, samples: &[f32]) -> Result<Vec<u8>> {
        let mut output = vec![0_u8; AUDIO_MAX_OPUS_PACKET_BYTES];
        let bytes = unsafe {
            libopus_sys::opus_encode_float(
                self.encoder,
                samples.as_ptr(),
                AUDIO_FRAME_SAMPLES_PER_CHANNEL as i32,
                output.as_mut_ptr(),
                output.len() as i32,
            )
        };
        if bytes < 0 {
            anyhow::bail!("Opus encode failed: error code {bytes}");
        }
        output.truncate(bytes as usize);
        Ok(output)
    }
}

#[cfg(all(target_os = "windows", feature = "audio-windows"))]
pub(crate) struct WasapiLoopbackCapture {
    _com: ComApartment,
    audio_client: windows::Win32::Media::Audio::IAudioClient,
    capture_client: windows::Win32::Media::Audio::IAudioCaptureClient,
    format: WasapiSampleFormat,
    pending: Vec<f32>,
    description: String,
}

#[cfg(all(target_os = "windows", feature = "audio-windows"))]
impl WasapiLoopbackCapture {
    pub(crate) fn new() -> Result<Self> {
        use windows::Win32::{
            Media::Audio::{
                AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, IAudioCaptureClient,
                IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender,
            },
            System::Com::{CLSCTX_ALL, CoCreateInstance},
        };

        let com = ComApartment::new()?;
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .context("failed to create Windows MMDeviceEnumerator")?;
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .context("failed to open default Windows render endpoint")?;
        let audio_client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .context("failed to activate default render endpoint audio client")?;
        let mix_format_ptr = unsafe { audio_client.GetMixFormat() }
            .context("failed to read default render endpoint mix format")?;
        let mix_format = unsafe { WasapiMixFormat::from_ptr(mix_format_ptr) };
        if let Err(err) = mix_format.validate_for_opus_rtp() {
            unsafe {
                windows::Win32::System::Com::CoTaskMemFree(Some(mix_format_ptr.cast()));
            }
            return Err(err);
        }

        let buffer_duration_100ns = 200_000_i64;
        let initialize_result = unsafe {
            audio_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                buffer_duration_100ns,
                0,
                mix_format_ptr,
                None,
            )
        };
        unsafe {
            windows::Win32::System::Com::CoTaskMemFree(Some(mix_format_ptr.cast()));
        }
        initialize_result.context("failed to initialize WASAPI shared loopback capture")?;
        let capture_client: IAudioCaptureClient = unsafe { audio_client.GetService() }
            .context("failed to acquire WASAPI capture client")?;
        unsafe { audio_client.Start() }.context("failed to start WASAPI loopback capture")?;

        Ok(Self {
            _com: com,
            audio_client,
            capture_client,
            format: mix_format.sample_format,
            pending: Vec::new(),
            description: format!(
                "WASAPI loopback {} Hz {}ch {:?}",
                mix_format.sample_rate, mix_format.channels, mix_format.sample_format
            ),
        })
    }

    fn read_available_packets(&mut self) -> Result<()> {
        use windows::Win32::Media::Audio::AUDCLNT_BUFFERFLAGS_SILENT;

        loop {
            let packet_frames = unsafe { self.capture_client.GetNextPacketSize() }
                .context("failed to query WASAPI loopback packet size")?;
            if packet_frames == 0 {
                return Ok(());
            }

            let mut data = std::ptr::null_mut();
            let mut frames = 0_u32;
            let mut flags = 0_u32;
            unsafe {
                self.capture_client
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
            }
            .context("failed to read WASAPI loopback packet")?;
            let result = self.copy_packet(
                data,
                frames,
                flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0,
            );
            unsafe { self.capture_client.ReleaseBuffer(frames) }
                .context("failed to release WASAPI loopback packet")?;
            result?;
        }
    }

    fn copy_packet(&mut self, data: *mut u8, frames: u32, silent: bool) -> Result<()> {
        let sample_count = frames as usize * usize::from(AUDIO_CHANNELS);
        if silent || data.is_null() {
            self.pending.extend(std::iter::repeat_n(0.0, sample_count));
            return Ok(());
        }

        match self.format {
            WasapiSampleFormat::Float32 => {
                let samples =
                    unsafe { std::slice::from_raw_parts(data.cast::<f32>(), sample_count) };
                self.pending.extend(samples.iter().copied());
            }
            WasapiSampleFormat::Int16 => {
                let samples =
                    unsafe { std::slice::from_raw_parts(data.cast::<i16>(), sample_count) };
                self.pending
                    .extend(samples.iter().map(|sample| f32::from(*sample) / 32768.0));
            }
            WasapiSampleFormat::Unsupported => {
                anyhow::bail!("cannot copy unsupported WASAPI sample format")
            }
        }
        Ok(())
    }
}

#[cfg(all(target_os = "windows", feature = "audio-windows"))]
impl Drop for WasapiLoopbackCapture {
    fn drop(&mut self) {
        unsafe {
            self.audio_client.Stop().ok();
        }
    }
}

#[cfg(all(target_os = "windows", feature = "audio-windows"))]
unsafe impl Send for WasapiLoopbackCapture {}

#[cfg(all(target_os = "windows", feature = "audio-windows"))]
impl PcmAudioCapture for WasapiLoopbackCapture {
    fn description(&self) -> &str {
        &self.description
    }

    fn next_frame(&mut self) -> Result<PcmAudioFrame> {
        let expected_samples = AUDIO_FRAME_SAMPLES_PER_CHANNEL * usize::from(AUDIO_CHANNELS);
        let deadline = std::time::Instant::now() + AUDIO_CAPTURE_WAIT_TIMEOUT;
        while self.pending.len() < expected_samples && std::time::Instant::now() < deadline {
            self.read_available_packets()?;
            if self.pending.len() < expected_samples {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        if self.pending.len() < expected_samples {
            self.pending.resize(expected_samples, 0.0);
        }

        let samples = self.pending.drain(..expected_samples).collect();
        Ok(PcmAudioFrame {
            samples,
            sample_rate: AUDIO_SAMPLE_RATE,
            channels: AUDIO_CHANNELS,
        })
    }
}

#[cfg(all(target_os = "windows", feature = "audio-windows"))]
struct ComApartment {
    uninitialize: bool,
}

#[cfg(all(target_os = "windows", feature = "audio-windows"))]
impl ComApartment {
    fn new() -> Result<Self> {
        use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        hr.ok().context("failed to initialize COM for WASAPI")?;
        Ok(Self { uninitialize: true })
    }
}

#[cfg(all(target_os = "windows", feature = "audio-windows"))]
impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.uninitialize {
            unsafe {
                windows::Win32::System::Com::CoUninitialize();
            }
        }
    }
}

#[cfg(all(target_os = "windows", feature = "audio-windows"))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum WasapiSampleFormat {
    Float32,
    Int16,
    Unsupported,
}

#[cfg(all(target_os = "windows", feature = "audio-windows"))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct WasapiMixFormat {
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
    block_align: u16,
    sample_format: WasapiSampleFormat,
}

#[cfg(all(target_os = "windows", feature = "audio-windows"))]
impl WasapiMixFormat {
    unsafe fn from_ptr(format: *const windows::Win32::Media::Audio::WAVEFORMATEX) -> Self {
        use windows::Win32::Media::{
            Audio::{WAVE_FORMAT_PCM, WAVEFORMATEXTENSIBLE},
            KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE},
            Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT},
        };

        let tag = unsafe { std::ptr::addr_of!((*format).wFormatTag).read_unaligned() };
        let sample_rate = unsafe { std::ptr::addr_of!((*format).nSamplesPerSec).read_unaligned() };
        let channels = unsafe { std::ptr::addr_of!((*format).nChannels).read_unaligned() };
        let bits_per_sample =
            unsafe { std::ptr::addr_of!((*format).wBitsPerSample).read_unaligned() };
        let block_align = unsafe { std::ptr::addr_of!((*format).nBlockAlign).read_unaligned() };

        let sample_format = if u32::from(tag) == WAVE_FORMAT_IEEE_FLOAT {
            WasapiSampleFormat::Float32
        } else if u32::from(tag) == WAVE_FORMAT_PCM {
            WasapiSampleFormat::Int16
        } else if u32::from(tag) == WAVE_FORMAT_EXTENSIBLE {
            let extensible = format.cast::<WAVEFORMATEXTENSIBLE>();
            let sub_format =
                unsafe { std::ptr::addr_of!((*extensible).SubFormat).read_unaligned() };
            if sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                WasapiSampleFormat::Float32
            } else if sub_format == KSDATAFORMAT_SUBTYPE_PCM {
                WasapiSampleFormat::Int16
            } else {
                WasapiSampleFormat::Unsupported
            }
        } else {
            WasapiSampleFormat::Unsupported
        };

        Self {
            sample_rate,
            channels,
            bits_per_sample,
            block_align,
            sample_format,
        }
    }

    fn validate_for_opus_rtp(&self) -> Result<()> {
        if self.sample_rate != AUDIO_SAMPLE_RATE || self.channels != AUDIO_CHANNELS {
            anyhow::bail!(
                "WASAPI mix format is {} Hz {}ch; Sunrise currently requires {} Hz {}ch",
                self.sample_rate,
                self.channels,
                AUDIO_SAMPLE_RATE,
                AUDIO_CHANNELS
            );
        }
        match (self.sample_format, self.bits_per_sample) {
            (WasapiSampleFormat::Float32, 32) | (WasapiSampleFormat::Int16, 16) => Ok(()),
            _ => anyhow::bail!(
                "unsupported WASAPI mix format {:?}/{} bits; expected float32 or int16",
                self.sample_format,
                self.bits_per_sample
            ),
        }
    }
}

pub(crate) struct AudioPacketizer {
    sequence: u16,
    encryption: Option<AudioEncryptionKey>,
}

impl AudioPacketizer {
    pub(crate) fn new() -> Self {
        Self::with_encryption(None)
    }

    pub(crate) fn with_encryption(encryption: Option<AudioEncryptionKey>) -> Self {
        Self {
            sequence: 1,
            encryption,
        }
    }

    pub(crate) fn packetize(&mut self, packet: &EncodedAudioPacket) -> Vec<u8> {
        let sequence = self.sequence;
        let payload = match self.encryption {
            Some(key) => encrypt_audio_payload(&key, sequence, &packet.payload),
            None => packet.payload.clone(),
        };
        let rtp = build_audio_rtp_packet(sequence, packet.timestamp, &payload);
        self.sequence = self.sequence.wrapping_add(1);
        rtp
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct AudioEncryptionKey {
    key_id: u32,
    key: [u8; 16],
}

impl AudioEncryptionKey {
    pub(crate) fn new(key_id: u32, key: [u8; 16]) -> Self {
        Self { key_id, key }
    }
}

fn encrypt_audio_payload(key: &AudioEncryptionKey, sequence: u16, payload: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(&key.key));
    let mut encrypted = pkcs7_pad(payload);
    let mut previous = audio_iv(key.key_id, sequence);

    for block in encrypted.chunks_exact_mut(16) {
        for (byte, iv_byte) in block.iter_mut().zip(previous) {
            *byte ^= iv_byte;
        }
        let block_array = GenericArray::from_mut_slice(block);
        cipher.encrypt_block(block_array);
        previous.copy_from_slice(block_array);
    }

    encrypted
}

fn pkcs7_pad(payload: &[u8]) -> Vec<u8> {
    let padding_len = 16 - (payload.len() % 16);
    let mut padded = Vec::with_capacity(payload.len() + padding_len);
    padded.extend_from_slice(payload);
    padded.extend(std::iter::repeat_n(padding_len as u8, padding_len));
    padded
}

fn audio_iv(key_id: u32, sequence: u16) -> [u8; 16] {
    let mut iv = [0_u8; 16];
    iv[..4].copy_from_slice(&key_id.wrapping_add(u32::from(sequence)).to_be_bytes());
    iv
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
    use aes::cipher::BlockDecrypt;

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
        let packet = packetizer.packetize(&source.next_packet().unwrap());

        assert_eq!(&packet[..2], &[0x80, RTP_AUDIO_PAYLOAD_TYPE]);
        assert_eq!(&packet[2..4], &1_u16.to_be_bytes());
        assert_eq!(&packet[4..8], &0_u32.to_be_bytes());
        assert_eq!(&packet[RTP_AUDIO_HEADER_LEN..], &[0xF8, 0xFF, 0xFE]);
    }

    #[test]
    fn captured_opus_source_encodes_pcm_frames_and_advances_timestamps() {
        let mut source = CapturedOpusSource::new(FakePcmCapture::new(), FakeOpusEncoder);

        let first = source.next_packet().unwrap();
        let second = source.next_packet().unwrap();

        assert_eq!(source.description(), "fake loopback via Opus");
        assert_eq!(source.packet_interval(), Duration::from_millis(20));
        assert_eq!(first.timestamp, 0);
        assert_eq!(first.payload, vec![0xAA, 0x80]);
        assert_eq!(second.timestamp, 960);
        assert_eq!(second.payload, vec![0xAA, 0x40]);
    }

    #[cfg(all(target_os = "windows", feature = "audio-windows"))]
    #[test]
    fn wasapi_mix_format_gate_accepts_only_current_opus_rtp_shape() {
        assert!(
            WasapiMixFormat {
                sample_rate: 48_000,
                channels: 2,
                bits_per_sample: 32,
                block_align: 8,
                sample_format: WasapiSampleFormat::Float32,
            }
            .validate_for_opus_rtp()
            .is_ok()
        );
        assert!(
            WasapiMixFormat {
                sample_rate: 44_100,
                channels: 2,
                bits_per_sample: 32,
                block_align: 8,
                sample_format: WasapiSampleFormat::Float32,
            }
            .validate_for_opus_rtp()
            .is_err()
        );
    }

    struct FakePcmCapture {
        next_sample: f32,
    }

    impl FakePcmCapture {
        fn new() -> Self {
            Self { next_sample: 1.0 }
        }
    }

    impl PcmAudioCapture for FakePcmCapture {
        fn description(&self) -> &str {
            "fake loopback"
        }

        fn next_frame(&mut self) -> Result<PcmAudioFrame> {
            let sample = self.next_sample;
            self.next_sample *= 0.5;
            Ok(PcmAudioFrame {
                samples: vec![sample; 960 * 2],
                sample_rate: 48_000,
                channels: 2,
            })
        }
    }

    struct FakeOpusEncoder;

    impl OpusFrameEncoder for FakeOpusEncoder {
        fn encode_float(&mut self, samples: &[f32]) -> Result<Vec<u8>> {
            let first = (samples[0] * 128.0) as u8;
            Ok(vec![0xAA, first])
        }
    }

    #[test]
    fn encrypts_audio_rtp_payload_with_launch_ri_key() {
        let key = AudioEncryptionKey::new(
            0x1020_3040,
            [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ],
        );
        let mut packetizer = AudioPacketizer::with_encryption(Some(key));
        let packet = packetizer.packetize(&EncodedAudioPacket {
            payload: vec![0xF8, 0xFF, 0xFE],
            timestamp: 0,
        });

        assert_eq!(
            &packet[..RTP_AUDIO_HEADER_LEN],
            &[
                0x80,
                RTP_AUDIO_PAYLOAD_TYPE,
                0,
                1,
                0,
                0,
                0,
                0,
                0x52,
                0x53,
                0x41,
                0x50
            ]
        );
        assert_ne!(&packet[RTP_AUDIO_HEADER_LEN..], &[0xF8, 0xFF, 0xFE]);
        assert_eq!(packet.len(), RTP_AUDIO_HEADER_LEN + 16);
        assert_eq!(
            decrypt_audio_payload_for_test(&key, 1, &packet[RTP_AUDIO_HEADER_LEN..]),
            vec![0xF8, 0xFF, 0xFE]
        );
    }

    fn decrypt_audio_payload_for_test(
        key: &AudioEncryptionKey,
        sequence: u16,
        ciphertext: &[u8],
    ) -> Vec<u8> {
        assert_eq!(ciphertext.len() % 16, 0);
        let cipher = Aes128::new(GenericArray::from_slice(&key.key));
        let mut plaintext = ciphertext.to_vec();
        let mut previous = audio_iv(key.key_id, sequence);

        for block in plaintext.chunks_exact_mut(16) {
            let current_ciphertext = <[u8; 16]>::try_from(&*block).unwrap();
            cipher.decrypt_block(GenericArray::from_mut_slice(block));
            for (byte, iv_byte) in block.iter_mut().zip(previous) {
                *byte ^= iv_byte;
            }
            previous = current_ciphertext;
        }

        let padding_len = *plaintext.last().unwrap() as usize;
        assert!((1..=16).contains(&padding_len));
        plaintext.truncate(plaintext.len() - padding_len);
        plaintext
    }
}
