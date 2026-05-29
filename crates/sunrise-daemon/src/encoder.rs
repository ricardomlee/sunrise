use std::{path::PathBuf, time::Duration};

use anyhow::Result;

use crate::capture::CaptureSourceOptions;
use crate::media::VideoSource;

#[derive(Debug, Clone)]
pub(crate) struct EncodeSmokeOptions {
    pub(crate) source: CaptureSourceOptions,
    pub(crate) output_path: PathBuf,
    pub(crate) ffmpeg_path: PathBuf,
    pub(crate) encoder: String,
    pub(crate) frame_count: u32,
    pub(crate) fps: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct EncodeSmokeReport {
    pub(crate) output_path: PathBuf,
    pub(crate) encoder: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) frames: u32,
    pub(crate) elapsed: Duration,
    pub(crate) bytes_written: u64,
    pub(crate) nal_units: usize,
    pub(crate) source_format: String,
}

#[derive(Debug, Clone)]
pub(crate) struct NativeNvencSmokeOptions {
    pub(crate) source: CaptureSourceOptions,
    pub(crate) output_path: PathBuf,
    pub(crate) frame_count: u32,
    pub(crate) fps: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct NativeNvencSmokeReport {
    pub(crate) output_path: PathBuf,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) frames: u32,
    pub(crate) elapsed: Duration,
    pub(crate) bytes_written: u64,
    pub(crate) nal_units: usize,
    pub(crate) source_format: String,
}

#[cfg(all(target_os = "windows", feature = "capture-windows"))]
pub(crate) fn run_encode_smoke(options: EncodeSmokeOptions) -> Result<EncodeSmokeReport> {
    ffmpeg_impl::run_encode_smoke(options)
}

#[cfg(all(target_os = "windows", feature = "capture-windows"))]
pub(crate) fn qsv_video_source_from_env() -> Result<Box<dyn VideoSource>> {
    ffmpeg_impl::hardware_video_source_from_env("h264_qsv")
}

#[cfg(all(target_os = "windows", feature = "capture-windows"))]
pub(crate) fn ffmpeg_nvenc_video_source_from_env() -> Result<Box<dyn VideoSource>> {
    ffmpeg_impl::hardware_video_source_from_env("h264_nvenc")
}

#[cfg(not(all(target_os = "windows", feature = "capture-windows")))]
pub(crate) fn run_encode_smoke(options: EncodeSmokeOptions) -> Result<EncodeSmokeReport> {
    let _ = (
        options.source.monitor_index,
        options.source.timeout_ms,
        options.output_path,
        options.ffmpeg_path,
        options.encoder,
        options.frame_count,
        options.fps,
    );
    anyhow::bail!(
        "encode-smoke requires Windows and the capture-windows feature; run: cargo run -p sunrise-daemon --features capture-windows -- encode-smoke"
    )
}

#[cfg(not(all(target_os = "windows", feature = "capture-windows")))]
pub(crate) fn qsv_video_source_from_env() -> Result<Box<dyn VideoSource>> {
    anyhow::bail!(
        "QSV video source requires Windows and the capture-windows feature; run: cargo run -p sunrise-daemon --features capture-windows"
    )
}

#[cfg(not(all(target_os = "windows", feature = "capture-windows")))]
pub(crate) fn ffmpeg_nvenc_video_source_from_env() -> Result<Box<dyn VideoSource>> {
    anyhow::bail!(
        "FFmpeg NVENC video source requires Windows and the capture-windows feature; run: cargo run -p sunrise-daemon --features capture-windows"
    )
}

#[cfg(all(target_os = "windows", feature = "native-nvenc"))]
pub(crate) fn run_native_nvenc_smoke(
    options: NativeNvencSmokeOptions,
) -> Result<NativeNvencSmokeReport> {
    native_nvenc_impl::run_native_nvenc_smoke(options)
}

#[cfg(all(target_os = "windows", feature = "native-nvenc"))]
pub(crate) fn native_nvenc_video_source_from_env() -> Result<Box<dyn VideoSource>> {
    native_nvenc_impl::native_nvenc_video_source_from_env()
}

#[cfg(not(all(target_os = "windows", feature = "native-nvenc")))]
pub(crate) fn run_native_nvenc_smoke(
    options: NativeNvencSmokeOptions,
) -> Result<NativeNvencSmokeReport> {
    let _ = (
        options.source.monitor_index,
        options.source.timeout_ms,
        options.output_path,
        options.frame_count,
        options.fps,
    );
    anyhow::bail!(
        "native-nvenc-smoke requires Windows and the native-nvenc feature; run: cargo run -p sunrise-daemon --features native-nvenc -- native-nvenc-smoke"
    )
}

#[cfg(not(all(target_os = "windows", feature = "native-nvenc")))]
pub(crate) fn native_nvenc_video_source_from_env() -> Result<Box<dyn VideoSource>> {
    anyhow::bail!(
        "native NVENC video source requires Windows and the native-nvenc feature; run: cargo run -p sunrise-daemon --features native-nvenc"
    )
}

#[cfg(all(target_os = "windows", feature = "capture-windows"))]
mod ffmpeg_impl {
    use std::{
        collections::VecDeque,
        fs,
        io::{Read, Write},
        path::PathBuf,
        process::{Child, ChildStdin, Command, Stdio},
        sync::mpsc::{self, Receiver},
        thread,
        time::Instant,
    };

    use anyhow::{Context, Result, bail};
    use tracing::{info, warn};

    use crate::capture::WindowsCaptureSource;
    use crate::media::{EncodedVideoFrame, VideoSource, split_annex_b_access_units};

    use super::{EncodeSmokeOptions, EncodeSmokeReport, h264_nal_unit_count};

    pub(crate) fn hardware_video_source_from_env(
        encoder: &'static str,
    ) -> Result<Box<dyn VideoSource>> {
        let fps = std::env::var("SUNRISE_VIDEO_FPS")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(30)
            .max(1);
        let monitor_index = std::env::var("SUNRISE_CAPTURE_MONITOR")
            .ok()
            .and_then(|value| value.parse::<usize>().ok());
        let timeout_ms = std::env::var("SUNRISE_CAPTURE_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(33)
            .max(1);
        let ffmpeg_path = std::env::var("SUNRISE_FFMPEG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("ffmpeg.exe"));

        Ok(Box::new(FfmpegHardwareVideoSource::new(
            ffmpeg_path,
            encoder,
            crate::capture::CaptureSourceOptions {
                monitor_index,
                timeout_ms,
            },
            fps,
        )?))
    }

    pub(crate) fn run_encode_smoke(options: EncodeSmokeOptions) -> Result<EncodeSmokeReport> {
        if options.encoder.eq_ignore_ascii_case("auto") {
            let mut nvenc_options = options.clone();
            nvenc_options.encoder = "h264_nvenc".to_string();
            match run_encode_smoke_once(nvenc_options) {
                Ok(report) => return Ok(report),
                Err(err) => {
                    warn!(
                        error = %err,
                        "h264_nvenc encode smoke failed; falling back to libx264"
                    );
                }
            }

            let mut software_options = options;
            software_options.encoder = "libx264".to_string();
            return run_encode_smoke_once(software_options);
        }

        run_encode_smoke_once(options)
    }

    fn run_encode_smoke_once(options: EncodeSmokeOptions) -> Result<EncodeSmokeReport> {
        if let Some(parent) = options.output_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create encoder smoke output directory {}",
                    parent.display()
                )
            })?;
        }

        let mut source = WindowsCaptureSource::new(options.source.clone())?;
        let first_frame = source.next_frame()?;
        let width = first_frame.width;
        let height = first_frame.height;
        let source_format = first_frame.source_format.clone();
        let frame_count = options.frame_count.max(1);
        let fps = options.fps.max(1);
        let args = ffmpeg_file_args(&options, width, height, fps);

        info!(
            ffmpeg = %options.ffmpeg_path.display(),
            encoder = %options.encoder,
            output = %options.output_path.display(),
            width,
            height,
            frames = frame_count,
            fps,
            source_format = %source_format,
            "starting H.264 encode smoke"
        );

        let started = Instant::now();
        let mut child = Command::new(&options.ffmpeg_path)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "failed to start ffmpeg at {}; install ffmpeg.exe or pass --ffmpeg",
                    options.ffmpeg_path.display()
                )
            })?;

        let mut stdin = child.stdin.take().context("failed to open ffmpeg stdin")?;
        if let Err(err) = stdin.write_all(&first_frame.bgra) {
            drop(stdin);
            return Err(ffmpeg_write_error(child, err, "first captured frame"));
        }
        for _ in 1..frame_count {
            let frame = source.next_frame()?;
            if frame.width != width || frame.height != height {
                bail!(
                    "capture frame size changed from {width}x{height} to {}x{}",
                    frame.width,
                    frame.height
                );
            }
            if let Err(err) = stdin.write_all(&frame.bgra) {
                drop(stdin);
                return Err(ffmpeg_write_error(child, err, "captured frame"));
            }
        }
        drop(stdin);

        let output = child
            .wait_with_output()
            .context("failed to wait for ffmpeg")?;
        let elapsed = started.elapsed();
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "ffmpeg exited with {}; stderr:\n{}",
                output.status,
                stderr.trim()
            );
        }

        let encoded = fs::read(&options.output_path).with_context(|| {
            format!(
                "failed to read encoded H.264 output {}",
                options.output_path.display()
            )
        })?;
        let nal_units = h264_nal_unit_count(&encoded);
        if nal_units == 0 {
            bail!("encoded H.264 output contained no Annex B NAL start codes");
        }
        let bytes_written = encoded.len() as u64;

        info!(
            output = %options.output_path.display(),
            encoder = %options.encoder,
            width,
            height,
            frames = frame_count,
            elapsed_ms = elapsed.as_millis(),
            bytes_written,
            nal_units,
            "H.264 encode smoke completed"
        );

        Ok(EncodeSmokeReport {
            output_path: options.output_path,
            encoder: options.encoder,
            width,
            height,
            frames: frame_count,
            elapsed,
            bytes_written,
            nal_units,
            source_format,
        })
    }

    struct FfmpegHardwareVideoSource {
        source: WindowsCaptureSource,
        child: Child,
        stdin: ChildStdin,
        frames: Receiver<Vec<u8>>,
        pending_input: Option<Vec<u8>>,
        description: String,
        width: u32,
        height: u32,
        frame_interval: std::time::Duration,
        timestamp_step: u32,
        frame_index: u32,
    }

    impl FfmpegHardwareVideoSource {
        fn new(
            ffmpeg_path: PathBuf,
            encoder: &'static str,
            source_options: crate::capture::CaptureSourceOptions,
            fps: u32,
        ) -> Result<Self> {
            let mut source = WindowsCaptureSource::new(source_options)?;
            let first_frame = source.next_frame()?;
            let width = first_frame.width;
            let height = first_frame.height;
            let source_format = first_frame.source_format.clone();
            let fps = fps.max(1);
            let args = ffmpeg_pipe_args(encoder, width, height, fps);

            info!(
                ffmpeg = %ffmpeg_path.display(),
                encoder,
                width,
                height,
                fps,
                source_format = %source_format,
                "starting live FFmpeg H.264 hardware encoder"
            );

            let mut child = Command::new(&ffmpeg_path)
                .args(&args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .with_context(|| {
                    format!(
                        "failed to start ffmpeg at {}; install ffmpeg.exe or set SUNRISE_FFMPEG",
                        ffmpeg_path.display()
                    )
                })?;
            let stdin = child.stdin.take().context("failed to open ffmpeg stdin")?;
            let stdout = child
                .stdout
                .take()
                .context("failed to open ffmpeg stdout")?;
            let (tx, rx) = mpsc::sync_channel(16);
            thread::spawn(move || read_annex_b_frames(stdout, tx));

            Ok(Self {
                source,
                child,
                stdin,
                frames: rx,
                pending_input: Some(first_frame.bgra),
                description: format!(
                    "FFmpeg live capture {width}x{height} {source_format} -> {encoder}"
                ),
                width,
                height,
                frame_interval: std::time::Duration::from_millis(u64::from(1000 / fps)),
                timestamp_step: 90_000 / fps,
                frame_index: 1,
            })
        }

        fn write_next_input_frame(&mut self) -> Result<()> {
            let pixels = match self.pending_input.take() {
                Some(pixels) => pixels,
                None => {
                    let frame = self.source.next_frame()?;
                    if frame.width != self.width || frame.height != self.height {
                        bail!(
                            "capture frame size changed from {}x{} to {}x{}",
                            self.width,
                            self.height,
                            frame.width,
                            frame.height
                        );
                    }
                    frame.bgra
                }
            };

            self.stdin
                .write_all(&pixels)
                .context("failed to write captured frame to QSV ffmpeg")?;
            Ok(())
        }
    }

    impl Drop for FfmpegHardwareVideoSource {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    impl VideoSource for FfmpegHardwareVideoSource {
        fn description(&self) -> &str {
            &self.description
        }

        fn frame_count_hint(&self) -> Option<usize> {
            None
        }

        fn frame_interval(&self) -> std::time::Duration {
            self.frame_interval
        }

        fn next_frame(&mut self) -> Result<EncodedVideoFrame> {
            let deadline = Instant::now() + std::time::Duration::from_secs(2);
            loop {
                self.write_next_input_frame()?;
                let now = Instant::now();
                if now >= deadline {
                    bail!("timed out waiting for QSV encoder output");
                }
                let wait = self.frame_interval.min(deadline - now);
                match self.frames.recv_timeout(wait) {
                    Ok(data) => {
                        let frame_index = self.frame_index;
                        let timestamp_90khz = frame_index.saturating_mul(self.timestamp_step);
                        self.frame_index = self.frame_index.wrapping_add(1);
                        return Ok(EncodedVideoFrame {
                            data,
                            frame_index,
                            timestamp_90khz,
                        });
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        bail!("QSV ffmpeg output stream closed")
                    }
                }
            }
        }
    }

    fn read_annex_b_frames<R: Read>(mut stdout: R, tx: mpsc::SyncSender<Vec<u8>>) {
        let mut scratch = [0_u8; 16 * 1024];
        let mut pending = Vec::new();
        let mut queued = VecDeque::new();
        loop {
            match stdout.read(&mut scratch) {
                Ok(0) => break,
                Ok(bytes) => {
                    pending.extend_from_slice(&scratch[..bytes]);
                    let frames = split_annex_b_access_units(&pending);
                    if frames.len() > 1 {
                        queued.extend(frames[..frames.len() - 1].iter().cloned());
                        pending = frames.last().cloned().unwrap_or_default();
                    }
                    while let Some(frame) = queued.pop_front() {
                        if tx.send(frame).is_err() {
                            return;
                        }
                    }
                }
                Err(_) => break,
            }
        }

        for frame in split_annex_b_access_units(&pending) {
            if tx.send(frame).is_err() {
                return;
            }
        }
    }

    fn ffmpeg_file_args(
        options: &EncodeSmokeOptions,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Vec<String> {
        let mut args = ffmpeg_input_args(&options.encoder, width, height, fps);
        append_encoder_args(&mut args, &options.encoder, fps);
        args.extend([
            "-vf".to_string(),
            encoder_filter(&options.encoder).to_string(),
            "-f".to_string(),
            "h264".to_string(),
            options.output_path.display().to_string(),
        ]);
        args
    }

    fn ffmpeg_pipe_args(encoder: &str, width: u32, height: u32, fps: u32) -> Vec<String> {
        let mut args = ffmpeg_input_args(encoder, width, height, fps);
        append_encoder_args(&mut args, encoder, fps);
        args.extend([
            "-vf".to_string(),
            encoder_filter(encoder).to_string(),
            "-f".to_string(),
            "h264".to_string(),
            "pipe:1".to_string(),
        ]);
        args
    }

    fn ffmpeg_input_args(encoder: &str, width: u32, height: u32, fps: u32) -> Vec<String> {
        let mut args = Vec::new();
        if uses_qsv(encoder) {
            args.extend([
                "-init_hw_device".to_string(),
                "qsv=hw".to_string(),
                "-filter_hw_device".to_string(),
                "hw".to_string(),
            ]);
        }

        args.extend([
            "-hide_banner".to_string(),
            "-loglevel".to_string(),
            "error".to_string(),
            "-y".to_string(),
            "-f".to_string(),
            "rawvideo".to_string(),
            "-pixel_format".to_string(),
            "bgra".to_string(),
            "-video_size".to_string(),
            format!("{width}x{height}"),
            "-framerate".to_string(),
            fps.to_string(),
            "-i".to_string(),
            "pipe:0".to_string(),
            "-an".to_string(),
        ]);
        args
    }

    fn ffmpeg_write_error(
        child: std::process::Child,
        err: std::io::Error,
        frame_context: &str,
    ) -> anyhow::Error {
        match child.wait_with_output() {
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::anyhow!(
                    "failed to write {frame_context} to ffmpeg: {err}; ffmpeg exited with {}; stderr:\n{}\nIf hardware encoding failed, try --encoder libx264 to validate the capture-to-H264 path without hardware acceleration.",
                    output.status,
                    stderr.trim()
                )
            }
            Err(wait_err) => anyhow::anyhow!(
                "failed to write {frame_context} to ffmpeg: {err}; also failed to collect ffmpeg stderr: {wait_err}"
            ),
        }
    }

    fn append_encoder_args(args: &mut Vec<String>, encoder: &str, fps: u32) {
        args.extend(["-c:v".to_string(), ffmpeg_encoder_name(encoder).to_string()]);
        if encoder.eq_ignore_ascii_case("h264_nvenc") {
            args.extend([
                "-preset".to_string(),
                "p1".to_string(),
                "-tune".to_string(),
                "ll".to_string(),
                "-rc".to_string(),
                "constqp".to_string(),
                "-qp".to_string(),
                "24".to_string(),
                "-bf".to_string(),
                "0".to_string(),
                "-g".to_string(),
                fps.to_string(),
            ]);
        } else if encoder.eq_ignore_ascii_case("libx264") {
            args.extend([
                "-preset".to_string(),
                "ultrafast".to_string(),
                "-tune".to_string(),
                "zerolatency".to_string(),
                "-x264-params".to_string(),
                format!("keyint={fps}:min-keyint={fps}:scenecut=0"),
            ]);
        } else if uses_qsv(encoder) {
            args.extend([
                "-preset".to_string(),
                "veryfast".to_string(),
                "-look_ahead".to_string(),
                "0".to_string(),
                "-bf".to_string(),
                "0".to_string(),
                "-g".to_string(),
                fps.to_string(),
            ]);
        }
    }

    fn encoder_filter(encoder: &str) -> &'static str {
        if uses_qsv(encoder) {
            "scale=trunc(iw/2)*2:trunc(ih/2)*2,format=nv12,hwupload=extra_hw_frames=64"
        } else {
            "scale=trunc(iw/2)*2:trunc(ih/2)*2,format=yuv420p"
        }
    }

    fn uses_qsv(encoder: &str) -> bool {
        encoder.eq_ignore_ascii_case("h264_qsv") || encoder.eq_ignore_ascii_case("qsv")
    }

    fn ffmpeg_encoder_name(encoder: &str) -> &str {
        if encoder.eq_ignore_ascii_case("qsv") {
            "h264_qsv"
        } else {
            encoder
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::capture::CaptureSourceOptions;

        #[test]
        fn builds_nvenc_ffmpeg_rawvideo_args() {
            let options = EncodeSmokeOptions {
                source: CaptureSourceOptions {
                    monitor_index: None,
                    timeout_ms: 33,
                },
                output_path: "out.h264".into(),
                ffmpeg_path: "ffmpeg.exe".into(),
                encoder: "h264_nvenc".to_string(),
                frame_count: 2,
                fps: 30,
            };

            let args = ffmpeg_file_args(&options, 1280, 720, 30);

            assert!(
                args.windows(2)
                    .any(|pair| pair == ["-pixel_format", "bgra"])
            );
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["-video_size", "1280x720"])
            );
            assert!(args.windows(2).any(|pair| pair == ["-c:v", "h264_nvenc"]));
            assert!(args.contains(&"scale=trunc(iw/2)*2:trunc(ih/2)*2,format=yuv420p".to_string()));
            assert_eq!(args.last().map(String::as_str), Some("out.h264"));
        }

        #[test]
        fn builds_qsv_ffmpeg_rawvideo_args() {
            let options = EncodeSmokeOptions {
                source: CaptureSourceOptions {
                    monitor_index: Some(2),
                    timeout_ms: 33,
                },
                output_path: "qsv.h264".into(),
                ffmpeg_path: "ffmpeg.exe".into(),
                encoder: "h264_qsv".to_string(),
                frame_count: 2,
                fps: 60,
            };

            let args = ffmpeg_file_args(&options, 1920, 1080, 60);

            assert!(
                args.windows(2)
                    .any(|pair| pair == ["-init_hw_device", "qsv=hw"])
            );
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["-filter_hw_device", "hw"])
            );
            assert!(args.windows(2).any(|pair| pair == ["-c:v", "h264_qsv"]));
            assert!(
                args.contains(
                    &"scale=trunc(iw/2)*2:trunc(ih/2)*2,format=nv12,hwupload=extra_hw_frames=64"
                        .to_string()
                )
            );
            assert!(args.windows(2).any(|pair| pair == ["-g", "60"]));
            assert_eq!(args.last().map(String::as_str), Some("qsv.h264"));
        }
    }
}

#[cfg(all(target_os = "windows", feature = "native-nvenc"))]
mod native_nvenc_impl {
    use std::{
        fs,
        io::Write,
        panic::AssertUnwindSafe,
        time::{Duration, Instant},
    };

    use anyhow::{Context, Result, anyhow, bail};
    use nvenc::{
        bitstream::BitStream,
        encoder::Encoder,
        session::{InitParams, NeedsConfig, Session},
        sys::{
            enums::{
                NVencBufferFormat, NVencParamsRcMode, NVencPicStruct, NVencPicType, NVencTuningInfo,
            },
            guids::{NV_ENC_CODEC_H264_GUID, NV_ENC_PRESET_P3_GUID},
        },
    };
    use tracing::{info, warn};
    use windows::{
        Win32::Graphics::{
            Direct3D::{
                D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, Fxc::D3DCompile, ID3DBlob, ID3DInclude,
            },
            Direct3D11::{
                D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_COMPARISON_NEVER,
                D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_SAMPLER_DESC, D3D11_TEXTURE_ADDRESS_CLAMP,
                D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT, ID3D11ClassInstance,
                ID3D11ClassLinkage, ID3D11DepthStencilView, ID3D11Device, ID3D11DeviceContext,
                ID3D11PixelShader, ID3D11RenderTargetView, ID3D11SamplerState, ID3D11Texture2D,
                ID3D11VertexShader,
            },
            Dxgi::Common::{
                DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
                DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, DXGI_SAMPLE_DESC,
            },
        },
        core::PCSTR,
    };
    use windows_capture::{
        dxgi_duplication_api::{
            DxgiDuplicationApi, DxgiDuplicationFormat, DxgiDuplicationFrame,
            Error as DxgiDuplicationError,
        },
        monitor::Monitor,
    };

    use crate::{
        capture::CaptureSourceOptions,
        media::{EncodedVideoFrame, VideoSource},
    };

    use super::{NativeNvencSmokeOptions, NativeNvencSmokeReport, h264_nal_unit_count};

    pub(crate) fn native_nvenc_video_source_from_env() -> Result<Box<dyn VideoSource>> {
        let fps = std::env::var("SUNRISE_VIDEO_FPS")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(30)
            .max(1);
        let monitor_index = std::env::var("SUNRISE_CAPTURE_MONITOR")
            .ok()
            .and_then(|value| value.parse::<usize>().ok());
        let timeout_ms = std::env::var("SUNRISE_CAPTURE_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(33)
            .max(1);

        Ok(Box::new(NativeNvencVideoSource::new(
            CaptureSourceOptions {
                monitor_index,
                timeout_ms,
            },
            fps,
        )?))
    }

    pub(crate) fn run_native_nvenc_smoke(
        options: NativeNvencSmokeOptions,
    ) -> Result<NativeNvencSmokeReport> {
        if let Some(parent) = options.output_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create native NVENC smoke output directory {}",
                    parent.display()
                )
            })?;
        }

        let mut source =
            DxgiTextureSource::new(options.source.monitor_index, options.source.timeout_ms)?;
        let first_frame = source.next_frame()?;
        let width = first_frame.width();
        let height = first_frame.height();
        let source_dxgi_format = first_frame.format();
        let source_format = format!("{source_dxgi_format:?}");
        let input_stage = NvencInputStage::new(
            first_frame.device(),
            first_frame.device_context(),
            width,
            height,
            source_dxgi_format,
        )?;
        input_stage.update_from_frame(&first_frame)?;
        drop(first_frame);

        let nvenc_format_name = input_stage.nvenc_format_name();
        let frame_count = options.frame_count.max(1);
        let fps = options.fps.max(1);

        let encoder = create_encoder(
            input_stage.device(),
            width,
            height,
            fps,
            input_stage.nvenc_format(),
        )?;
        let mut bitstreams = BitstreamPool::new(&encoder)?;
        let mut out = fs::File::create(&options.output_path).with_context(|| {
            format!(
                "failed to create native NVENC output {}",
                options.output_path.display()
            )
        })?;

        info!(
            output = %options.output_path.display(),
            width,
            height,
            frames = frame_count,
            fps,
            source_format = %source_format,
            nvenc_format = %nvenc_format_name,
            "starting native D3D11 zero-copy NVENC smoke"
        );

        let started = Instant::now();
        encode_input_stage(&encoder, &mut bitstreams, &mut out, &input_stage, 0)?;
        let mut reused_frames = 0_u32;

        for frame_index in 1..frame_count {
            match source.next_frame() {
                Ok(frame) => {
                    if frame.width() != width || frame.height() != height {
                        bail!(
                            "capture frame size changed from {width}x{height} to {}x{}",
                            frame.width(),
                            frame.height()
                        );
                    }
                    input_stage.update_from_frame(&frame)?;
                }
                Err(err) if should_reuse_last_frame(&err) => {
                    reused_frames += 1;
                }
                Err(err) => {
                    return Err(err).context("failed to acquire a DXGI texture frame");
                }
            }
            encode_input_stage(
                &encoder,
                &mut bitstreams,
                &mut out,
                &input_stage,
                frame_index as usize,
            )?;
        }

        flush_encoder(&encoder, &mut bitstreams, &mut out)?;
        out.flush().context("failed to flush native NVENC output")?;
        let elapsed = started.elapsed();

        let encoded = fs::read(&options.output_path).with_context(|| {
            format!(
                "failed to read native NVENC output {}",
                options.output_path.display()
            )
        })?;
        let nal_units = h264_nal_unit_count(&encoded);
        if nal_units == 0 {
            bail!("native NVENC output contained no Annex B NAL start codes");
        }
        let bytes_written = encoded.len() as u64;

        info!(
            output = %options.output_path.display(),
            width,
            height,
            frames = frame_count,
            elapsed_ms = elapsed.as_millis(),
            bytes_written,
            nal_units,
            reused_frames,
            "native D3D11 zero-copy NVENC smoke completed"
        );

        Ok(NativeNvencSmokeReport {
            output_path: options.output_path,
            width,
            height,
            frames: frame_count,
            elapsed,
            bytes_written,
            nal_units,
            source_format,
        })
    }

    struct DxgiTextureSource {
        duplication: DxgiDuplicationApi,
        timeout_ms: u32,
    }

    impl DxgiTextureSource {
        fn new(monitor_index: Option<usize>, timeout_ms: u32) -> Result<Self> {
            let selected = create_dxgi_texture_source(monitor_index)?;
            info!(
                monitor_index = selected.info.index,
                monitor_name = %selected.info.name,
                device_name = %selected.info.device_name,
                adapter = %selected.info.device_string,
                width = selected.info.width,
                height = selected.info.height,
                refresh_rate = selected.info.refresh_rate,
                "starting zero-copy DXGI texture source"
            );
            Ok(Self {
                duplication: selected.duplication,
                timeout_ms: timeout_ms.max(1),
            })
        }

        fn next_frame(
            &mut self,
        ) -> std::result::Result<DxgiDuplicationFrame<'_>, DxgiDuplicationError> {
            self.duplication.acquire_next_frame(self.timeout_ms)
        }
    }

    struct SelectedDxgiTextureSource {
        duplication: DxgiDuplicationApi,
        info: MonitorInfo,
    }

    #[derive(Clone, Debug)]
    struct MonitorInfo {
        index: usize,
        name: String,
        device_name: String,
        device_string: String,
        width: u32,
        height: u32,
        refresh_rate: u32,
    }

    fn create_dxgi_texture_source(
        monitor_index: Option<usize>,
    ) -> Result<SelectedDxgiTextureSource> {
        let candidates = candidate_monitors(monitor_index)?;
        let mut failures = Vec::new();

        for monitor in candidates {
            let info = monitor_info(monitor);
            info!(
                monitor_index = info.index,
                monitor_name = %info.name,
                device_name = %info.device_name,
                adapter = %info.device_string,
                width = info.width,
                height = info.height,
                refresh_rate = info.refresh_rate,
                "probing zero-copy DXGI texture monitor"
            );
            match DxgiDuplicationApi::new(monitor) {
                Ok(duplication) => return Ok(SelectedDxgiTextureSource { duplication, info }),
                Err(err) => {
                    warn!(
                        monitor_index = info.index,
                        monitor_name = %info.name,
                        device_name = %info.device_name,
                        adapter = %info.device_string,
                        error = %err,
                        "DXGI duplication rejected monitor"
                    );
                    failures.push(format!(
                        "#{} {} {}: {err}",
                        info.index, info.device_name, info.device_string
                    ));
                }
            }
        }

        bail!(
            "failed to create DXGI duplication session for active monitors: {}",
            failures.join("; ")
        )
    }

    fn candidate_monitors(monitor_index: Option<usize>) -> Result<Vec<Monitor>> {
        if let Some(index) = monitor_index {
            return Ok(vec![
                Monitor::from_index(index)
                    .with_context(|| format!("failed to select monitor {index}"))?,
            ]);
        }

        let mut monitors = Vec::new();
        if let Ok(primary) = Monitor::primary() {
            monitors.push(primary);
        }
        for monitor in Monitor::enumerate().context("failed to enumerate active monitors")? {
            if !monitors.contains(&monitor) {
                monitors.push(monitor);
            }
        }

        if monitors.is_empty() {
            bail!("no active Windows monitors found");
        }
        Ok(monitors)
    }

    fn monitor_info(monitor: Monitor) -> MonitorInfo {
        MonitorInfo {
            index: monitor.index().unwrap_or(0),
            name: monitor.name().unwrap_or_else(|_| "unknown".to_string()),
            device_name: monitor
                .device_name()
                .unwrap_or_else(|_| "unknown".to_string()),
            device_string: monitor
                .device_string()
                .unwrap_or_else(|_| "unknown".to_string()),
            width: monitor.width().unwrap_or(0),
            height: monitor.height().unwrap_or(0),
            refresh_rate: monitor.refresh_rate().unwrap_or(0),
        }
    }

    struct NativeNvencVideoSource {
        source: DxgiTextureSource,
        input_stage: NvencInputStage,
        encoder: Encoder,
        bitstreams: BitstreamPool,
        description: String,
        width: u32,
        height: u32,
        frame_interval: Duration,
        timestamp_step: u32,
        frame_index: u32,
        encode_index: usize,
        reused_frames: u64,
    }

    // The NVENC session, D3D11 resources, and bitstream pool are owned by one video source and are
    // accessed only through `&mut self` on the RTP sender task. This marker lets Tokio move that
    // task between worker threads without permitting concurrent access to the encoder.
    unsafe impl Send for NativeNvencVideoSource {}

    impl NativeNvencVideoSource {
        fn new(options: CaptureSourceOptions, fps: u32) -> Result<Self> {
            let mut source = DxgiTextureSource::new(options.monitor_index, options.timeout_ms)?;
            let first_frame = source.next_frame()?;
            let width = first_frame.width();
            let height = first_frame.height();
            let source_dxgi_format = first_frame.format();
            let source_format = format!("{source_dxgi_format:?}");
            let input_stage = NvencInputStage::new(
                first_frame.device(),
                first_frame.device_context(),
                width,
                height,
                source_dxgi_format,
            )?;
            input_stage.update_from_frame(&first_frame)?;
            drop(first_frame);

            let fps = fps.max(1);
            let encoder = create_encoder(
                input_stage.device(),
                width,
                height,
                fps,
                input_stage.nvenc_format(),
            )?;
            let bitstreams = BitstreamPool::new(&encoder)?;
            let description = format!(
                "native D3D11 NVENC live capture {width}x{height} {source_format} -> {}",
                input_stage.nvenc_format_name()
            );

            info!(
                width,
                height,
                fps,
                source_format = %source_format,
                nvenc_format = %input_stage.nvenc_format_name(),
                "created native D3D11 NVENC live video source"
            );

            Ok(Self {
                source,
                input_stage,
                encoder,
                bitstreams,
                description,
                width,
                height,
                frame_interval: Duration::from_millis(u64::from(1000 / fps)),
                timestamp_step: 90_000 / fps,
                frame_index: 1,
                encode_index: 0,
                reused_frames: 0,
            })
        }

        fn refresh_input_stage(&mut self) -> Result<()> {
            match self.source.next_frame() {
                Ok(frame) => {
                    if frame.width() != self.width || frame.height() != self.height {
                        bail!(
                            "capture frame size changed from {}x{} to {}x{}",
                            self.width,
                            self.height,
                            frame.width(),
                            frame.height()
                        );
                    }
                    self.input_stage.update_from_frame(&frame)?;
                }
                Err(err) if should_reuse_last_frame(&err) => {
                    self.reused_frames += 1;
                }
                Err(err) => {
                    return Err(err).context("failed to acquire a DXGI texture frame");
                }
            }
            Ok(())
        }
    }

    impl VideoSource for NativeNvencVideoSource {
        fn description(&self) -> &str {
            &self.description
        }

        fn frame_count_hint(&self) -> Option<usize> {
            None
        }

        fn frame_interval(&self) -> Duration {
            self.frame_interval
        }

        fn next_frame(&mut self) -> Result<EncodedVideoFrame> {
            if self.encode_index > 0 {
                self.refresh_input_stage()?;
            }

            let data = encode_input_stage_to_vec(
                &self.encoder,
                &mut self.bitstreams,
                &self.input_stage,
                self.encode_index,
            )?;
            let frame_index = self.frame_index;
            let timestamp_90khz = frame_index.saturating_mul(self.timestamp_step);

            self.encode_index = self.encode_index.wrapping_add(1);
            self.frame_index = self.frame_index.wrapping_add(1);

            Ok(EncodedVideoFrame {
                data,
                frame_index,
                timestamp_90khz,
            })
        }
    }

    enum NvencInputStage {
        Copy(D3D11CopyFrameBuffer),
        Convert(Bgra8GpuConverter),
    }

    impl NvencInputStage {
        fn new(
            device: &ID3D11Device,
            context: &ID3D11DeviceContext,
            width: u32,
            height: u32,
            source_format: DxgiDuplicationFormat,
        ) -> Result<Self> {
            if needs_bgra_gpu_conversion(source_format) {
                Ok(Self::Convert(Bgra8GpuConverter::new(
                    device, context, width, height,
                )?))
            } else {
                Ok(Self::Copy(D3D11CopyFrameBuffer::new(
                    device,
                    context,
                    width,
                    height,
                    source_format,
                )?))
            }
        }

        fn update_from_frame(&self, frame: &DxgiDuplicationFrame<'_>) -> Result<()> {
            match self {
                Self::Copy(buffer) => buffer.copy_from(frame.texture(), frame.format()),
                Self::Convert(converter) => converter.convert(frame.texture()).map(|_| ()),
            }
        }

        fn device(&self) -> &ID3D11Device {
            match self {
                Self::Copy(buffer) => &buffer.device,
                Self::Convert(converter) => &converter.device,
            }
        }

        fn texture(&self) -> &ID3D11Texture2D {
            match self {
                Self::Copy(buffer) => &buffer.texture,
                Self::Convert(converter) => &converter.output_texture,
            }
        }

        fn nvenc_format(&self) -> NVencBufferFormat {
            match self {
                Self::Copy(buffer) => buffer.nvenc_format(),
                Self::Convert(_) => NVencBufferFormat::ARGB,
            }
        }

        fn nvenc_format_name(&self) -> &'static str {
            match self {
                Self::Copy(buffer) => buffer.nvenc_format_name(),
                Self::Convert(_) => "ARGB via GPU BGRA8 conversion",
            }
        }

        fn pitch(&self) -> u32 {
            match self {
                Self::Copy(buffer) => buffer.width * 4,
                Self::Convert(converter) => converter.width * 4,
            }
        }
    }

    struct D3D11CopyFrameBuffer {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        texture: ID3D11Texture2D,
        source_format: DxgiDuplicationFormat,
        width: u32,
    }

    impl D3D11CopyFrameBuffer {
        fn new(
            device: &ID3D11Device,
            context: &ID3D11DeviceContext,
            width: u32,
            height: u32,
            source_format: DxgiDuplicationFormat,
        ) -> Result<Self> {
            let mut texture = None;
            let texture_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: copy_texture_format(source_format)?,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            unsafe {
                device
                    .CreateTexture2D(&texture_desc, None, Some(&mut texture))
                    .context("failed to create persistent NVENC input texture")?;
            }
            let texture = texture.context("D3D11 returned no persistent NVENC input texture")?;

            Ok(Self {
                device: device.clone(),
                context: context.clone(),
                texture,
                source_format,
                width,
            })
        }

        fn copy_from(
            &self,
            input: &ID3D11Texture2D,
            input_format: DxgiDuplicationFormat,
        ) -> Result<()> {
            if input_format != self.source_format {
                bail!(
                    "capture frame format changed from {:?} to {:?}",
                    self.source_format,
                    input_format
                );
            }
            unsafe {
                self.context.CopyResource(&self.texture, input);
            }
            Ok(())
        }

        fn nvenc_format(&self) -> NVencBufferFormat {
            nvenc_format_for_dxgi(self.source_format)
                .expect("copy input stage only accepts NVENC-compatible DXGI formats")
        }

        fn nvenc_format_name(&self) -> &'static str {
            nvenc_format_name(self.source_format, false)
                .expect("copy input stage only accepts NVENC-compatible DXGI formats")
        }
    }

    struct Bgra8GpuConverter {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        output_texture: ID3D11Texture2D,
        render_target: ID3D11RenderTargetView,
        vertex_shader: ID3D11VertexShader,
        pixel_shader: ID3D11PixelShader,
        sampler: ID3D11SamplerState,
        width: u32,
        height: u32,
    }

    impl Bgra8GpuConverter {
        fn new(
            device: &ID3D11Device,
            context: &ID3D11DeviceContext,
            width: u32,
            height: u32,
        ) -> Result<Self> {
            let mut output_texture = None;
            let texture_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            unsafe {
                device
                    .CreateTexture2D(&texture_desc, None, Some(&mut output_texture))
                    .context("failed to create GPU BGRA conversion texture")?;
            }
            let output_texture =
                output_texture.context("D3D11 returned no GPU BGRA conversion texture")?;

            let mut render_target = None;
            unsafe {
                device
                    .CreateRenderTargetView(&output_texture, None, Some(&mut render_target))
                    .context("failed to create GPU BGRA conversion render target")?;
            }
            let render_target =
                render_target.context("D3D11 returned no GPU BGRA conversion render target")?;

            let vertex_bytecode = compile_shader(
                GPU_CONVERTER_HLSL,
                c"vs_main".as_ptr().cast(),
                c"vs_5_0".as_ptr().cast(),
            )
            .context("failed to compile GPU conversion vertex shader")?;
            let pixel_bytecode = compile_shader(
                GPU_CONVERTER_HLSL,
                c"ps_main".as_ptr().cast(),
                c"ps_5_0".as_ptr().cast(),
            )
            .context("failed to compile GPU conversion pixel shader")?;

            let mut vertex_shader = None;
            let mut pixel_shader = None;
            unsafe {
                device
                    .CreateVertexShader(
                        &vertex_bytecode,
                        None::<&ID3D11ClassLinkage>,
                        Some(&mut vertex_shader),
                    )
                    .context("failed to create GPU conversion vertex shader")?;
                device
                    .CreatePixelShader(
                        &pixel_bytecode,
                        None::<&ID3D11ClassLinkage>,
                        Some(&mut pixel_shader),
                    )
                    .context("failed to create GPU conversion pixel shader")?;
            }
            let vertex_shader =
                vertex_shader.context("D3D11 returned no GPU conversion vertex shader")?;
            let pixel_shader =
                pixel_shader.context("D3D11 returned no GPU conversion pixel shader")?;

            let sampler_desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MipLODBias: 0.0,
                MaxAnisotropy: 1,
                ComparisonFunc: D3D11_COMPARISON_NEVER,
                BorderColor: [0.0; 4],
                MinLOD: 0.0,
                MaxLOD: f32::MAX,
            };
            let mut sampler = None;
            unsafe {
                device
                    .CreateSamplerState(&sampler_desc, Some(&mut sampler))
                    .context("failed to create GPU conversion sampler")?;
            }
            let sampler = sampler.context("D3D11 returned no GPU conversion sampler")?;

            info!(
                width,
                height,
                output_format = "Bgra8",
                "created GPU texture converter for NVENC input"
            );

            Ok(Self {
                device: device.clone(),
                context: context.clone(),
                output_texture,
                render_target,
                vertex_shader,
                pixel_shader,
                sampler,
                width,
                height,
            })
        }

        fn convert(&self, input: &ID3D11Texture2D) -> Result<&ID3D11Texture2D> {
            let mut shader_resource = None;
            unsafe {
                self.device
                    .CreateShaderResourceView(input, None, Some(&mut shader_resource))
                    .context("failed to create GPU conversion shader resource")?;
            }
            let shader_resource =
                shader_resource.context("D3D11 returned no GPU conversion shader resource view")?;

            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: self.width as f32,
                Height: self.height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };

            unsafe {
                self.context.RSSetViewports(Some(&[viewport]));
                self.context.IASetInputLayout(None);
                self.context
                    .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                self.context
                    .VSSetShader(&self.vertex_shader, None::<&[Option<ID3D11ClassInstance>]>);
                self.context
                    .PSSetShader(&self.pixel_shader, None::<&[Option<ID3D11ClassInstance>]>);
                self.context
                    .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
                self.context
                    .PSSetShaderResources(0, Some(&[Some(shader_resource)]));
                self.context
                    .OMSetRenderTargets(Some(&[Some(self.render_target.clone())]), None);
                self.context.Draw(3, 0);
                self.context.PSSetShaderResources(0, Some(&[None]));
                self.context
                    .OMSetRenderTargets(Some(&[None]), None::<&ID3D11DepthStencilView>);
            }

            Ok(&self.output_texture)
        }
    }

    struct BitstreamPool {
        buffers: Vec<BitStream>,
    }

    impl BitstreamPool {
        fn new(encoder: &Encoder) -> Result<Self> {
            Ok(Self {
                buffers: vec![
                    encoder.create_bitstream_buffer().map_err(nvenc_error)?,
                    encoder.create_bitstream_buffer().map_err(nvenc_error)?,
                ],
            })
        }

        fn next(&mut self) -> Result<BitStream> {
            self.buffers
                .pop()
                .ok_or_else(|| anyhow!("native NVENC bitstream pool was empty"))
        }

        fn recycle(&mut self, bitstream: BitStream) {
            self.buffers.push(bitstream);
        }
    }

    fn create_encoder(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
        format: NVencBufferFormat,
    ) -> Result<Encoder> {
        if !nvenc_runtime_visible() {
            bail!(
                "failed to find NVIDIA NVENC runtime nvEncodeAPI64.dll on PATH or in Windows system directories; install or repair the NVIDIA display driver with NVENC support"
            );
        }
        let session: Session<NeedsConfig> =
            std::panic::catch_unwind(AssertUnwindSafe(|| Session::open_dx(device)))
                .map_err(|_| {
                    anyhow!(
                        "failed to load NVIDIA NVENC runtime nvEncodeAPI64.dll; install or repair the NVIDIA display driver with NVENC support"
                    )
                })?
                .map_err(nvenc_error)?;
        let (session, mut config) = session
            .get_encode_preset_config_ex(
                NV_ENC_CODEC_H264_GUID,
                NV_ENC_PRESET_P3_GUID,
                NVencTuningInfo::UltraLowLatency,
            )
            .map_err(nvenc_error)?;

        config.preset_cfg.rc_params.rate_control_mode = NVencParamsRcMode::VBR;
        config.preset_cfg.rc_params.average_bit_rate = 20_000_000;
        config.preset_cfg.gop_len = fps;
        config.preset_cfg.frame_interval_p = 1;

        let init_params = InitParams {
            encode_guid: NV_ENC_CODEC_H264_GUID,
            preset_guid: NV_ENC_PRESET_P3_GUID,
            resolution: [width, height],
            aspect_ratio: [width, height],
            frame_rate: [fps, 1],
            tuning_info: NVencTuningInfo::UltraLowLatency,
            buffer_format: format,
            encode_config: &mut config.preset_cfg,
            enable_ptd: true,
            max_encoder_resolution: [0, 0],
        };

        session.init_encoder(init_params).map_err(nvenc_error)
    }

    fn nvenc_runtime_visible() -> bool {
        let dll_name = "nvEncodeAPI64.dll";
        if std::env::current_dir()
            .map(|path| path.join(dll_name).is_file())
            .unwrap_or(false)
        {
            return true;
        }

        if let Some(system_root) = std::env::var_os("SystemRoot") {
            let system_root = std::path::PathBuf::from(system_root);
            if system_root.join("System32").join(dll_name).is_file()
                || system_root.join("SysWOW64").join(dll_name).is_file()
            {
                return true;
            }
        }

        std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).any(|entry| entry.join(dll_name).is_file()))
            .unwrap_or(false)
    }

    fn should_reuse_last_frame(error: &DxgiDuplicationError) -> bool {
        matches!(error, DxgiDuplicationError::Timeout)
    }

    fn encode_input_stage(
        encoder: &Encoder,
        bitstreams: &mut BitstreamPool,
        out: &mut fs::File,
        input_stage: &NvencInputStage,
        frame_index: usize,
    ) -> Result<()> {
        let bytes = encode_input_stage_to_vec(encoder, bitstreams, input_stage, frame_index)?;
        out.write_all(&bytes)
            .context("failed to write native NVENC bitstream")?;
        Ok(())
    }

    fn encode_input_stage_to_vec(
        encoder: &Encoder,
        bitstreams: &mut BitstreamPool,
        input_stage: &NvencInputStage,
        frame_index: usize,
    ) -> Result<Vec<u8>> {
        let registered = encoder
            .register_resource_dx11(
                input_stage.texture(),
                input_stage.nvenc_format(),
                input_stage.pitch(),
            )
            .map_err(nvenc_error)?;
        let bitstream = bitstreams.next()?;
        encoder
            .encode_picture(
                &registered,
                &bitstream,
                frame_index,
                frame_index as u64,
                input_stage.nvenc_format(),
                NVencPicStruct::Frame,
                if frame_index == 0 {
                    NVencPicType::IDR
                } else {
                    NVencPicType::P
                },
                None,
            )
            .map_err(nvenc_error)?;
        drop(registered);

        let data = bitstream_bytes(&bitstream)?;
        bitstreams.recycle(bitstream);
        Ok(data)
    }

    fn flush_encoder(
        encoder: &Encoder,
        bitstreams: &mut BitstreamPool,
        out: &mut fs::File,
    ) -> Result<()> {
        encoder.end_encode().map_err(nvenc_error)?;
        let bitstream = bitstreams.next()?;
        write_bitstream(out, &bitstream)?;
        bitstreams.recycle(bitstream);
        Ok(())
    }

    fn write_bitstream(out: &mut fs::File, bitstream: &BitStream) -> Result<()> {
        let data = bitstream_bytes(bitstream)?;
        out.write_all(&data)
            .context("failed to write native NVENC bitstream")?;
        Ok(())
    }

    fn bitstream_bytes(bitstream: &BitStream) -> Result<Vec<u8>> {
        let lock = bitstream.try_lock(true).map_err(nvenc_error)?;
        Ok(lock.as_slice().to_vec())
    }

    fn nvenc_format_for_dxgi(format: DxgiDuplicationFormat) -> Result<NVencBufferFormat> {
        match format {
            DxgiDuplicationFormat::Bgra8 | DxgiDuplicationFormat::Bgra8Srgb => {
                Ok(NVencBufferFormat::ARGB)
            }
            DxgiDuplicationFormat::Rgba8 | DxgiDuplicationFormat::Rgba8Srgb => {
                Ok(NVencBufferFormat::ABGR)
            }
            DxgiDuplicationFormat::Rgba16F
            | DxgiDuplicationFormat::Rgb10A2
            | DxgiDuplicationFormat::Rgb10XrA2 => {
                bail!(
                    "native NVENC zero-copy currently needs an 8-bit desktop surface; got {format:?}"
                )
            }
        }
    }

    fn copy_texture_format(format: DxgiDuplicationFormat) -> Result<DXGI_FORMAT> {
        match format {
            DxgiDuplicationFormat::Bgra8 => Ok(DXGI_FORMAT_B8G8R8A8_UNORM),
            DxgiDuplicationFormat::Bgra8Srgb => Ok(DXGI_FORMAT_B8G8R8A8_UNORM_SRGB),
            DxgiDuplicationFormat::Rgba8 => Ok(DXGI_FORMAT_R8G8B8A8_UNORM),
            DxgiDuplicationFormat::Rgba8Srgb => Ok(DXGI_FORMAT_R8G8B8A8_UNORM_SRGB),
            DxgiDuplicationFormat::Rgba16F
            | DxgiDuplicationFormat::Rgb10A2
            | DxgiDuplicationFormat::Rgb10XrA2 => {
                bail!("HDR/10-bit formats require the GPU BGRA conversion stage")
            }
        }
    }

    fn needs_bgra_gpu_conversion(format: DxgiDuplicationFormat) -> bool {
        matches!(
            format,
            DxgiDuplicationFormat::Rgba16F
                | DxgiDuplicationFormat::Rgb10A2
                | DxgiDuplicationFormat::Rgb10XrA2
        )
    }

    fn nvenc_format_name(
        format: DxgiDuplicationFormat,
        using_gpu_converter: bool,
    ) -> Result<&'static str> {
        if using_gpu_converter {
            return Ok("ARGB via GPU BGRA8 conversion");
        }
        match format {
            DxgiDuplicationFormat::Bgra8 | DxgiDuplicationFormat::Bgra8Srgb => Ok("ARGB"),
            DxgiDuplicationFormat::Rgba8 | DxgiDuplicationFormat::Rgba8Srgb => Ok("ABGR"),
            DxgiDuplicationFormat::Rgba16F
            | DxgiDuplicationFormat::Rgb10A2
            | DxgiDuplicationFormat::Rgb10XrA2 => {
                bail!(
                    "native NVENC zero-copy currently needs an 8-bit desktop surface; got {format:?}"
                )
            }
        }
    }

    fn compile_shader(source: &[u8], entry: *const u8, target: *const u8) -> Result<Vec<u8>> {
        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        let result = unsafe {
            D3DCompile(
                source.as_ptr().cast(),
                source.len(),
                PCSTR(c"sunrise_gpu_converter.hlsl".as_ptr().cast()),
                None,
                None::<&ID3DInclude>,
                PCSTR(entry),
                PCSTR(target),
                0,
                0,
                &mut code,
                Some(&mut errors),
            )
        };
        if let Err(err) = result {
            let message = errors
                .as_ref()
                .map(blob_string)
                .unwrap_or_else(|| "no shader compiler diagnostics".to_string());
            return Err(anyhow!("D3DCompile failed: {err}; {message}"));
        }
        let code = code.context("D3DCompile returned no shader bytecode")?;
        let bytes = unsafe {
            std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize())
        };
        Ok(bytes.to_vec())
    }

    fn blob_string(blob: &ID3DBlob) -> String {
        let bytes = unsafe {
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
        };
        String::from_utf8_lossy(bytes).trim().to_string()
    }

    const GPU_CONVERTER_HLSL: &[u8] = br#"
struct VsOut {
    float4 position : SV_POSITION;
    float2 uv : TEXCOORD0;
};

VsOut vs_main(uint vertex_id : SV_VertexID) {
    float2 positions[3] = {
        float2(-1.0, -1.0),
        float2(-1.0,  3.0),
        float2( 3.0, -1.0)
    };
    float2 uvs[3] = {
        float2(0.0, 1.0),
        float2(0.0, -1.0),
        float2(2.0, 1.0)
    };

    VsOut output;
    output.position = float4(positions[vertex_id], 0.0, 1.0);
    output.uv = uvs[vertex_id];
    return output;
}

Texture2D<float4> source_texture : register(t0);
SamplerState source_sampler : register(s0);

float4 ps_main(VsOut input) : SV_Target {
    float4 color = source_texture.Sample(source_sampler, input.uv);
    color.rgb = max(color.rgb, 0.0);
    color.rgb = color.rgb / (1.0 + color.rgb);
    color.rgb = pow(saturate(color.rgb), 1.0 / 2.2);
    return float4(color.rgb, 1.0);
}
"#;

    fn nvenc_error(error: nvenc::sys::result::NVencError) -> anyhow::Error {
        anyhow!("NVENC error: {error:?}")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn maps_dxgi_bgra_to_nvenc_argb() {
            assert_eq!(
                nvenc_format_name(DxgiDuplicationFormat::Bgra8, false).unwrap(),
                "ARGB"
            );
        }

        #[test]
        fn maps_dxgi_rgba_to_nvenc_abgr() {
            assert_eq!(
                nvenc_format_name(DxgiDuplicationFormat::Rgba8, false).unwrap(),
                "ABGR"
            );
        }

        #[test]
        fn routes_hdr_formats_through_gpu_converter() {
            assert!(needs_bgra_gpu_conversion(DxgiDuplicationFormat::Rgba16F));
            assert_eq!(
                nvenc_format_name(DxgiDuplicationFormat::Rgba16F, true).unwrap(),
                "ARGB via GPU BGRA8 conversion"
            );
        }

        #[test]
        fn reuses_last_frame_when_dxgi_times_out() {
            assert!(should_reuse_last_frame(&DxgiDuplicationError::Timeout));
        }
    }
}

#[cfg(any(test, all(target_os = "windows", feature = "capture-windows")))]
fn h264_nal_unit_count(data: &[u8]) -> usize {
    annex_b_start_codes(data).count()
}

#[cfg(any(test, all(target_os = "windows", feature = "capture-windows")))]
fn annex_b_start_codes(data: &[u8]) -> impl Iterator<Item = usize> + '_ {
    AnnexBStartCodes { data, index: 0 }
}

#[cfg(any(test, all(target_os = "windows", feature = "capture-windows")))]
struct AnnexBStartCodes<'a> {
    data: &'a [u8],
    index: usize,
}

#[cfg(any(test, all(target_os = "windows", feature = "capture-windows")))]
impl Iterator for AnnexBStartCodes<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        while self.index + 3 <= self.data.len() {
            let index = self.index;
            if self.data[index..].starts_with(&[0, 0, 0, 1]) {
                self.index += 4;
                return Some(index);
            }
            if self.data[index..].starts_with(&[0, 0, 1]) {
                self.index += 3;
                return Some(index);
            }
            self.index += 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_annex_b_nal_units() {
        assert_eq!(
            h264_nal_unit_count(&[0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2]),
            2
        );
        assert_eq!(h264_nal_unit_count(&[1, 2, 3]), 0);
    }
}
