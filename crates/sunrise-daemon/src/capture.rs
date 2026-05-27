use std::path::PathBuf;

use anyhow::Result;

#[derive(Debug, Clone)]
pub(crate) struct CaptureSmokeOptions {
    pub(crate) output_path: PathBuf,
    pub(crate) monitor_index: Option<usize>,
    pub(crate) timeout_ms: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct CaptureSmokeReport {
    pub(crate) output_path: PathBuf,
    pub(crate) monitor_index: usize,
    pub(crate) monitor_name: Option<String>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) row_pitch: u32,
    pub(crate) depth_pitch: u32,
    pub(crate) color_format: String,
    pub(crate) bytes_written: usize,
}

#[cfg(all(target_os = "windows", feature = "capture-windows"))]
pub(crate) fn run_capture_smoke(options: CaptureSmokeOptions) -> Result<CaptureSmokeReport> {
    windows_capture_impl::run_capture_smoke(options)
}

#[cfg(not(all(target_os = "windows", feature = "capture-windows")))]
pub(crate) fn run_capture_smoke(options: CaptureSmokeOptions) -> Result<CaptureSmokeReport> {
    let _ = (
        options.output_path,
        options.monitor_index,
        options.timeout_ms,
    );
    anyhow::bail!(
        "Windows capture smoke requires Windows and the capture-windows feature; run: cargo run -p sunrise-daemon --features capture-windows -- capture-smoke"
    )
}

#[cfg(all(target_os = "windows", feature = "capture-windows"))]
mod windows_capture_impl {
    use std::{fs, path::Path, thread, time::Duration};

    use anyhow::{Context, Result, bail};
    use tracing::info;
    use windows_capture::{
        dxgi_duplication_api::{DxgiDuplicationApi, DxgiDuplicationFormat},
        monitor::Monitor,
    };

    use super::{CaptureSmokeOptions, CaptureSmokeReport};

    pub(crate) fn run_capture_smoke(options: CaptureSmokeOptions) -> Result<CaptureSmokeReport> {
        if let Some(parent) = options.output_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create capture smoke output directory {}",
                    parent.display()
                )
            })?;
        }

        let monitor = match options.monitor_index {
            Some(index) => Monitor::from_index(index)
                .with_context(|| format!("failed to select monitor {index}"))?,
            None => Monitor::primary().context("failed to select primary monitor")?,
        };
        let monitor_index = monitor
            .index()
            .unwrap_or(options.monitor_index.unwrap_or(1));
        let monitor_name = monitor.name().ok();
        let width = monitor.width().context("failed to query monitor width")?;
        let height = monitor.height().context("failed to query monitor height")?;

        info!(
            monitor_index,
            monitor_name = monitor_name.as_deref().unwrap_or("unknown"),
            width,
            height,
            "starting Windows capture smoke"
        );

        let mut duplication = DxgiDuplicationApi::new(monitor)
            .context("failed to create DXGI duplication session")?;
        let timeout_ms = options.timeout_ms.max(1);
        let mut last_error = None;
        for attempt in 1..=30 {
            match duplication.acquire_next_frame(timeout_ms) {
                Ok(mut frame) => {
                    let frame_buffer = frame.buffer().context("failed to map capture frame")?;
                    let width = frame_buffer.width();
                    let height = frame_buffer.height();
                    let row_pitch = frame_buffer.row_pitch();
                    let depth_pitch = frame_buffer.depth_pitch();
                    let color_format = frame_buffer.format();
                    let mut packed_storage = Vec::new();
                    let packed_pixels = frame_buffer.as_nopadding_buffer(&mut packed_storage);
                    let bytes_written = write_frame_bmp(
                        &options.output_path,
                        width,
                        height,
                        color_format,
                        packed_pixels,
                    )
                    .with_context(|| {
                        format!(
                            "failed to save capture smoke frame to {}",
                            options.output_path.display()
                        )
                    })?;
                    info!(
                        attempt,
                        output = %options.output_path.display(),
                        row_pitch,
                        depth_pitch,
                        color_format = ?color_format,
                        bytes_written,
                        "captured Windows frame"
                    );
                    return Ok(CaptureSmokeReport {
                        output_path: options.output_path,
                        monitor_index,
                        monitor_name,
                        width,
                        height,
                        row_pitch,
                        depth_pitch,
                        color_format: format!("{color_format:?}"),
                        bytes_written,
                    });
                }
                Err(err) => {
                    last_error = Some(err);
                    thread::sleep(Duration::from_millis(16));
                }
            }
        }

        match last_error {
            Some(err) => Err(err).context("failed to acquire a Windows capture frame"),
            None => anyhow::bail!("failed to acquire a Windows capture frame"),
        }
    }

    fn write_frame_bmp(
        path: &Path,
        width: u32,
        height: u32,
        format: DxgiDuplicationFormat,
        pixels: &[u8],
    ) -> Result<usize> {
        let bytes_per_pixel = match format {
            DxgiDuplicationFormat::Bgra8
            | DxgiDuplicationFormat::Bgra8Srgb
            | DxgiDuplicationFormat::Rgba8
            | DxgiDuplicationFormat::Rgba8Srgb => 4,
            DxgiDuplicationFormat::Rgb10A2
            | DxgiDuplicationFormat::Rgb10XrA2
            | DxgiDuplicationFormat::Rgba16F => {
                bail!(
                    "capture format {format:?} is not a byte BGRA/RGBA surface; HDR/10-bit conversion is a later capture milestone"
                )
            }
        };
        let pixel_len = width as usize * height as usize * bytes_per_pixel;
        let Some(pixels) = pixels.get(..pixel_len) else {
            bail!(
                "capture buffer is too small: got {} bytes, expected {pixel_len}",
                pixels.len()
            );
        };

        let bmp_pixels = match format {
            DxgiDuplicationFormat::Bgra8 | DxgiDuplicationFormat::Bgra8Srgb => pixels.to_vec(),
            DxgiDuplicationFormat::Rgba8 | DxgiDuplicationFormat::Rgba8Srgb => rgba_to_bgra(pixels),
            DxgiDuplicationFormat::Rgb10A2
            | DxgiDuplicationFormat::Rgb10XrA2
            | DxgiDuplicationFormat::Rgba16F => unreachable!("non-8-bit formats bail above"),
        };

        let header_len = 14_u32 + 40_u32;
        let file_size = header_len
            .checked_add(bmp_pixels.len() as u32)
            .context("BMP file would be too large")?;
        let mut bmp = Vec::with_capacity(file_size as usize);
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&file_size.to_le_bytes());
        bmp.extend_from_slice(&0_u16.to_le_bytes());
        bmp.extend_from_slice(&0_u16.to_le_bytes());
        bmp.extend_from_slice(&header_len.to_le_bytes());

        bmp.extend_from_slice(&40_u32.to_le_bytes());
        bmp.extend_from_slice(&(width as i32).to_le_bytes());
        // Negative height makes the DIB top-down, matching the capture buffer row order.
        bmp.extend_from_slice(&(-(height as i32)).to_le_bytes());
        bmp.extend_from_slice(&1_u16.to_le_bytes());
        bmp.extend_from_slice(&32_u16.to_le_bytes());
        bmp.extend_from_slice(&0_u32.to_le_bytes());
        bmp.extend_from_slice(&(bmp_pixels.len() as u32).to_le_bytes());
        bmp.extend_from_slice(&0_i32.to_le_bytes());
        bmp.extend_from_slice(&0_i32.to_le_bytes());
        bmp.extend_from_slice(&0_u32.to_le_bytes());
        bmp.extend_from_slice(&0_u32.to_le_bytes());
        bmp.extend_from_slice(&bmp_pixels);

        fs::write(path, bmp)?;
        Ok(bmp_pixels.len())
    }

    fn rgba_to_bgra(pixels: &[u8]) -> Vec<u8> {
        let mut converted = pixels.to_vec();
        for pixel in converted.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        converted
    }

    #[cfg(test)]
    mod tests {
        use std::fs;
        use std::time::{SystemTime, UNIX_EPOCH};

        use anyhow::Result;

        use super::*;

        #[test]
        fn writes_top_down_bgra_bmp() -> Result<()> {
            let path = temp_bmp_path("bgra");
            let pixels = [1, 2, 3, 255, 4, 5, 6, 255];

            let bytes = write_frame_bmp(&path, 2, 1, DxgiDuplicationFormat::Bgra8, &pixels)?;
            let bmp = fs::read(&path)?;
            let _ = fs::remove_file(&path);

            assert_eq!(bytes, pixels.len());
            assert_eq!(&bmp[0..2], b"BM");
            assert_eq!(i32::from_le_bytes(bmp[22..26].try_into()?), -1);
            assert_eq!(&bmp[54..], &pixels);
            Ok(())
        }

        #[test]
        fn converts_rgba_bmp_pixels_to_bgra() -> Result<()> {
            let path = temp_bmp_path("rgba");
            let pixels = [10, 20, 30, 255];

            write_frame_bmp(&path, 1, 1, DxgiDuplicationFormat::Rgba8, &pixels)?;
            let bmp = fs::read(&path)?;
            let _ = fs::remove_file(&path);

            assert_eq!(&bmp[54..], &[30, 20, 10, 255]);
            Ok(())
        }

        fn temp_bmp_path(name: &str) -> std::path::PathBuf {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after UNIX_EPOCH")
                .as_nanos();
            std::env::temp_dir().join(format!(
                "sunrise-capture-{name}-{}-{nanos}.bmp",
                std::process::id()
            ))
        }
    }
}
