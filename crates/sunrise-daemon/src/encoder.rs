use std::{path::PathBuf, time::Duration};

use anyhow::Result;

use crate::capture::CaptureSourceOptions;

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

#[cfg(all(target_os = "windows", feature = "capture-windows"))]
pub(crate) fn run_encode_smoke(options: EncodeSmokeOptions) -> Result<EncodeSmokeReport> {
    ffmpeg_impl::run_encode_smoke(options)
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

#[cfg(all(target_os = "windows", feature = "capture-windows"))]
mod ffmpeg_impl {
    use std::{
        fs,
        io::Write,
        process::{Command, Stdio},
        time::Instant,
    };

    use anyhow::{Context, Result, bail};
    use tracing::info;

    use crate::capture::WindowsCaptureSource;

    use super::{EncodeSmokeOptions, EncodeSmokeReport, h264_nal_unit_count};

    pub(crate) fn run_encode_smoke(options: EncodeSmokeOptions) -> Result<EncodeSmokeReport> {
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
        let args = ffmpeg_args(&options, width, height, fps);

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

        {
            let mut stdin = child.stdin.take().context("failed to open ffmpeg stdin")?;
            stdin
                .write_all(&first_frame.bgra)
                .context("failed to write first captured frame to ffmpeg")?;
            for _ in 1..frame_count {
                let frame = source.next_frame()?;
                if frame.width != width || frame.height != height {
                    bail!(
                        "capture frame size changed from {width}x{height} to {}x{}",
                        frame.width,
                        frame.height
                    );
                }
                stdin
                    .write_all(&frame.bgra)
                    .context("failed to write captured frame to ffmpeg")?;
            }
        }

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

    fn ffmpeg_args(options: &EncodeSmokeOptions, width: u32, height: u32, fps: u32) -> Vec<String> {
        let mut args = vec![
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
        ];

        append_encoder_args(&mut args, &options.encoder, fps);
        args.extend([
            "-vf".to_string(),
            "scale=trunc(iw/2)*2:trunc(ih/2)*2,format=yuv420p".to_string(),
            "-f".to_string(),
            "h264".to_string(),
            options.output_path.display().to_string(),
        ]);
        args
    }

    fn append_encoder_args(args: &mut Vec<String>, encoder: &str, fps: u32) {
        args.extend(["-c:v".to_string(), encoder.to_string()]);
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

            let args = ffmpeg_args(&options, 1280, 720, 30);

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
