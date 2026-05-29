use std::{path::PathBuf, time::Duration};

use anyhow::Result;

#[cfg(all(target_os = "windows", feature = "capture-windows"))]
#[allow(unused_imports)]
pub(crate) use windows_capture_impl::WindowsCaptureSource;

#[derive(Debug, Clone)]
pub(crate) struct CaptureSourceOptions {
    pub(crate) monitor_index: Option<usize>,
    pub(crate) timeout_ms: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct CaptureSmokeOptions {
    pub(crate) output_path: PathBuf,
    pub(crate) source: CaptureSourceOptions,
}

#[derive(Debug, Clone)]
pub(crate) struct CaptureLoopOptions {
    pub(crate) source: CaptureSourceOptions,
    pub(crate) frame_count: u32,
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
    pub(crate) source_format: String,
    pub(crate) bytes_written: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct CaptureLoopReport {
    pub(crate) monitor_index: usize,
    pub(crate) monitor_name: Option<String>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) frames: u32,
    pub(crate) elapsed: Duration,
    pub(crate) source_format: String,
}

#[cfg(all(target_os = "windows", feature = "capture-windows"))]
#[derive(Debug, Clone)]
pub(crate) struct CapturedVideoFrame {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) stride: u32,
    pub(crate) row_pitch: u32,
    pub(crate) depth_pitch: u32,
    pub(crate) source_format: String,
    pub(crate) frame_index: u64,
    pub(crate) bgra: Vec<u8>,
}

#[cfg(all(target_os = "windows", feature = "capture-windows"))]
pub(crate) fn run_capture_smoke(options: CaptureSmokeOptions) -> Result<CaptureSmokeReport> {
    windows_capture_impl::run_capture_smoke(options)
}

#[cfg(not(all(target_os = "windows", feature = "capture-windows")))]
pub(crate) fn run_capture_smoke(options: CaptureSmokeOptions) -> Result<CaptureSmokeReport> {
    let _ = (
        options.output_path,
        options.source.monitor_index,
        options.source.timeout_ms,
    );
    anyhow::bail!(
        "Windows capture smoke requires Windows and the capture-windows feature; run: cargo run -p sunrise-daemon --features capture-windows -- capture-smoke"
    )
}

#[cfg(all(target_os = "windows", feature = "capture-windows"))]
pub(crate) fn run_wgc_smoke(options: CaptureSmokeOptions) -> Result<CaptureSmokeReport> {
    windows_capture_impl::run_wgc_smoke(options)
}

#[cfg(not(all(target_os = "windows", feature = "capture-windows")))]
pub(crate) fn run_wgc_smoke(options: CaptureSmokeOptions) -> Result<CaptureSmokeReport> {
    let _ = (
        options.output_path,
        options.source.monitor_index,
        options.source.timeout_ms,
    );
    anyhow::bail!(
        "Windows Graphics Capture smoke requires Windows and the capture-windows feature; run: cargo run -p sunrise-daemon --features capture-windows -- wgc-smoke"
    )
}

#[cfg(all(target_os = "windows", feature = "capture-windows"))]
pub(crate) fn run_capture_loop(options: CaptureLoopOptions) -> Result<CaptureLoopReport> {
    windows_capture_impl::run_capture_loop(options)
}

#[cfg(not(all(target_os = "windows", feature = "capture-windows")))]
pub(crate) fn run_capture_loop(options: CaptureLoopOptions) -> Result<CaptureLoopReport> {
    let _ = (
        options.source.monitor_index,
        options.source.timeout_ms,
        options.frame_count,
    );
    anyhow::bail!(
        "Windows capture loop requires Windows and the capture-windows feature; run: cargo run -p sunrise-daemon --features capture-windows -- capture-loop"
    )
}

#[cfg(all(target_os = "windows", feature = "capture-windows"))]
pub(crate) fn run_capture_list() -> Result<()> {
    windows_capture_impl::run_capture_list()
}

#[cfg(not(all(target_os = "windows", feature = "capture-windows")))]
pub(crate) fn run_capture_list() -> Result<()> {
    anyhow::bail!(
        "Windows capture monitor listing requires Windows and the capture-windows feature; run: cargo run -p sunrise-daemon --features capture-windows -- capture-list"
    )
}

#[cfg(all(target_os = "windows", feature = "capture-windows"))]
mod windows_capture_impl {
    use std::{
        fs,
        path::Path,
        sync::mpsc::{self, Receiver, SyncSender},
        thread,
        time::{Duration, Instant},
    };

    use anyhow::{Context, Result, bail};
    use tracing::{info, warn};
    use windows_capture::{
        capture::{Context as CaptureContext, GraphicsCaptureApiHandler},
        dxgi_duplication_api::{DxgiDuplicationApi, DxgiDuplicationFormat},
        frame::Frame,
        graphics_capture_api::InternalCaptureControl,
        monitor::Monitor,
        settings::{
            ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
            MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
        },
    };

    use super::{
        CaptureLoopOptions, CaptureLoopReport, CaptureSmokeOptions, CaptureSmokeReport,
        CaptureSourceOptions, CapturedVideoFrame,
    };

    pub(crate) fn run_capture_list() -> Result<()> {
        let monitors = Monitor::enumerate().context("failed to enumerate active monitors")?;
        if monitors.is_empty() {
            println!("No active Windows monitors found.");
            return Ok(());
        }

        for monitor in monitors {
            let info = monitor_info(monitor);
            let duplication = match DxgiDuplicationApi::new(monitor) {
                Ok(_) => "ok".to_string(),
                Err(err) => format!("failed: {err}"),
            };
            println!(
                "#{index} device={device_name} name={name} adapter={device_string} size={width}x{height} refresh={refresh_rate} duplication={duplication}",
                index = info.index,
                device_name = info.device_name,
                name = info.name,
                device_string = info.device_string,
                width = info.width,
                height = info.height,
                refresh_rate = info.refresh_rate,
            );
        }

        Ok(())
    }

    pub(crate) fn run_capture_smoke(options: CaptureSmokeOptions) -> Result<CaptureSmokeReport> {
        if let Some(parent) = options.output_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create capture smoke output directory {}",
                    parent.display()
                )
            })?;
        }

        let mut source = WindowsCaptureSource::new(options.source)?;
        let frame = source.next_frame()?;
        let bytes_written =
            write_bgra_bmp(&options.output_path, frame.width, frame.height, &frame.bgra)
                .with_context(|| {
                    format!(
                        "failed to save capture smoke frame to {}",
                        options.output_path.display()
                    )
                })?;

        info!(
            output = %options.output_path.display(),
            monitor_index = source.monitor_index(),
            monitor_name = source.monitor_name().unwrap_or("unknown"),
            width = frame.width,
            height = frame.height,
            stride = frame.stride,
            row_pitch = frame.row_pitch,
            depth_pitch = frame.depth_pitch,
            source_format = %frame.source_format,
            bytes_written,
            frame_index = frame.frame_index,
            "captured Windows frame"
        );

        Ok(CaptureSmokeReport {
            output_path: options.output_path,
            monitor_index: source.monitor_index(),
            monitor_name: source.monitor_name().map(str::to_string),
            width: frame.width,
            height: frame.height,
            row_pitch: frame.row_pitch,
            depth_pitch: frame.depth_pitch,
            source_format: frame.source_format,
            bytes_written,
        })
    }

    pub(crate) fn run_capture_loop(options: CaptureLoopOptions) -> Result<CaptureLoopReport> {
        let frame_count = options.frame_count.max(1);
        let mut source = WindowsCaptureSource::new(options.source)?;
        let started = Instant::now();
        let mut last = None;

        for _ in 0..frame_count {
            last = Some(source.next_frame()?);
        }

        let elapsed = started.elapsed();
        let frame = last.context("capture loop produced no frames")?;
        let fps = f64::from(frame_count) / elapsed.as_secs_f64().max(0.001);
        info!(
            monitor_index = source.monitor_index(),
            monitor_name = source.monitor_name().unwrap_or("unknown"),
            width = frame.width,
            height = frame.height,
            stride = frame.stride,
            frames = frame_count,
            elapsed_ms = elapsed.as_millis(),
            fps,
            source_format = %frame.source_format,
            "Windows capture loop completed"
        );

        Ok(CaptureLoopReport {
            monitor_index: source.monitor_index(),
            monitor_name: source.monitor_name().map(str::to_string),
            width: frame.width,
            height: frame.height,
            frames: frame_count,
            elapsed,
            source_format: frame.source_format,
        })
    }

    pub(crate) fn run_wgc_smoke(options: CaptureSmokeOptions) -> Result<CaptureSmokeReport> {
        if let Some(parent) = options.output_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create WGC smoke output directory {}",
                    parent.display()
                )
            })?;
        }

        let mut last_error = None;
        for monitor in candidate_monitors(options.source.monitor_index)? {
            let info = monitor_info(monitor);
            info!(
                monitor_index = info.index,
                monitor_name = %info.name,
                device_name = %info.device_name,
                adapter = %info.device_string,
                width = info.width,
                height = info.height,
                refresh_rate = info.refresh_rate,
                "probing Windows Graphics Capture monitor"
            );
            match capture_one_wgc_frame(monitor, options.source.timeout_ms) {
                Ok(frame) => {
                    let bytes_written = write_bgra_bmp(
                        &options.output_path,
                        frame.width,
                        frame.height,
                        &frame.bgra,
                    )
                    .with_context(|| {
                        format!(
                            "failed to save WGC smoke frame to {}",
                            options.output_path.display()
                        )
                    })?;
                    info!(
                        output = %options.output_path.display(),
                        monitor_index = info.index,
                        monitor_name = %info.name,
                        device_name = %info.device_name,
                        adapter = %info.device_string,
                        width = frame.width,
                        height = frame.height,
                        stride = frame.stride,
                        row_pitch = frame.row_pitch,
                        depth_pitch = frame.depth_pitch,
                        source_format = %frame.source_format,
                        bytes_written,
                        "captured Windows Graphics Capture frame"
                    );

                    return Ok(CaptureSmokeReport {
                        output_path: options.output_path,
                        monitor_index: info.index,
                        monitor_name: Some(info.name),
                        width: frame.width,
                        height: frame.height,
                        row_pitch: frame.row_pitch,
                        depth_pitch: frame.depth_pitch,
                        source_format: frame.source_format,
                        bytes_written,
                    });
                }
                Err(err) => {
                    warn!(
                        monitor_index = info.index,
                        monitor_name = %info.name,
                        device_name = %info.device_name,
                        adapter = %info.device_string,
                        error = %err,
                        "Windows Graphics Capture rejected monitor"
                    );
                    last_error = Some(format!("#{} {}: {err}", info.index, info.device_name));
                }
            }
        }

        bail!(
            "failed to capture a Windows Graphics Capture frame: {}",
            last_error.unwrap_or_else(|| "no active monitors found".to_string())
        )
    }

    pub(crate) struct WindowsCaptureSource {
        inner: WindowsCaptureSourceKind,
    }

    enum WindowsCaptureSourceKind {
        Dxgi(DxgiCaptureSource),
        Wgc(WgcCaptureSource),
    }

    impl WindowsCaptureSource {
        pub(crate) fn new(options: CaptureSourceOptions) -> Result<Self> {
            let inner = match capture_backend_from_env()? {
                CaptureBackend::Dxgi => {
                    WindowsCaptureSourceKind::Dxgi(DxgiCaptureSource::new(options)?)
                }
                CaptureBackend::Wgc => {
                    WindowsCaptureSourceKind::Wgc(WgcCaptureSource::new(options)?)
                }
                CaptureBackend::Auto => match DxgiCaptureSource::new(options.clone()) {
                    Ok(source) => WindowsCaptureSourceKind::Dxgi(source),
                    Err(err) => {
                        warn!(
                            error = %err,
                            "DXGI capture source failed; falling back to Windows Graphics Capture"
                        );
                        WindowsCaptureSourceKind::Wgc(WgcCaptureSource::new(options)?)
                    }
                },
            };
            Ok(Self { inner })
        }

        pub(crate) fn monitor_index(&self) -> usize {
            match &self.inner {
                WindowsCaptureSourceKind::Dxgi(source) => source.monitor_index,
                WindowsCaptureSourceKind::Wgc(source) => source.monitor_index,
            }
        }

        pub(crate) fn monitor_name(&self) -> Option<&str> {
            match &self.inner {
                WindowsCaptureSourceKind::Dxgi(source) => source.monitor_name.as_deref(),
                WindowsCaptureSourceKind::Wgc(source) => source.monitor_name.as_deref(),
            }
        }

        pub(crate) fn next_frame(&mut self) -> Result<CapturedVideoFrame> {
            match &mut self.inner {
                WindowsCaptureSourceKind::Dxgi(source) => source.next_frame(),
                WindowsCaptureSourceKind::Wgc(source) => source.next_frame(),
            }
        }
    }

    struct DxgiCaptureSource {
        duplication: DxgiDuplicationApi,
        monitor_index: usize,
        monitor_name: Option<String>,
        timeout_ms: u32,
        frame_index: u64,
    }

    impl DxgiCaptureSource {
        fn new(options: CaptureSourceOptions) -> Result<Self> {
            let selected = create_duplication_source(options.monitor_index)?;

            info!(
                monitor_index = selected.info.index,
                monitor_name = %selected.info.name,
                device_name = %selected.info.device_name,
                adapter = %selected.info.device_string,
                width = selected.info.width,
                height = selected.info.height,
                "starting Windows capture source"
            );

            Ok(Self {
                duplication: selected.duplication,
                monitor_index: selected.info.index,
                monitor_name: Some(selected.info.name),
                timeout_ms: options.timeout_ms.max(1),
                frame_index: 0,
            })
        }

        fn next_frame(&mut self) -> Result<CapturedVideoFrame> {
            let mut last_error = None;
            for _ in 1..=30 {
                match self.duplication.acquire_next_frame(self.timeout_ms) {
                    Ok(mut frame) => {
                        let frame_buffer = frame.buffer().context("failed to map capture frame")?;
                        let width = frame_buffer.width();
                        let height = frame_buffer.height();
                        let row_pitch = frame_buffer.row_pitch();
                        let depth_pitch = frame_buffer.depth_pitch();
                        let source_format = frame_buffer.format();
                        let mut packed_storage = Vec::new();
                        let packed_pixels = frame_buffer.as_nopadding_buffer(&mut packed_storage);
                        let bgra =
                            capture_pixels_to_bgra8(width, height, source_format, packed_pixels)?;

                        self.frame_index = self.frame_index.wrapping_add(1);
                        return Ok(CapturedVideoFrame {
                            width,
                            height,
                            stride: width * 4,
                            row_pitch,
                            depth_pitch,
                            source_format: format!("{source_format:?}"),
                            frame_index: self.frame_index,
                            bgra,
                        });
                    }
                    Err(err) => {
                        last_error = Some(err);
                        thread::sleep(std::time::Duration::from_millis(16));
                    }
                }
            }

            match last_error {
                Some(err) => Err(err).context("failed to acquire a Windows capture frame"),
                None => bail!("failed to acquire a Windows capture frame"),
            }
        }
    }

    struct WgcCaptureSource {
        control: Option<windows_capture::capture::CaptureControl<WgcFrameCapture, String>>,
        rx: Receiver<WgcFrameResult>,
        monitor_index: usize,
        monitor_name: Option<String>,
        timeout: Duration,
    }

    impl WgcCaptureSource {
        fn new(options: CaptureSourceOptions) -> Result<Self> {
            let mut last_error = None;
            for monitor in candidate_monitors(options.monitor_index)? {
                let info = monitor_info(monitor);
                info!(
                    monitor_index = info.index,
                    monitor_name = %info.name,
                    device_name = %info.device_name,
                    adapter = %info.device_string,
                    width = info.width,
                    height = info.height,
                    refresh_rate = info.refresh_rate,
                    "starting Windows Graphics Capture source"
                );
                match start_wgc_capture(monitor, false) {
                    Ok((control, rx)) => {
                        return Ok(Self {
                            control: Some(control),
                            rx,
                            monitor_index: info.index,
                            monitor_name: Some(info.name),
                            timeout: Duration::from_millis(u64::from(options.timeout_ms.max(1000))),
                        });
                    }
                    Err(err) => {
                        warn!(
                            monitor_index = info.index,
                            monitor_name = %info.name,
                            device_name = %info.device_name,
                            adapter = %info.device_string,
                            error = %err,
                            "Windows Graphics Capture rejected monitor"
                        );
                        last_error = Some(format!("#{} {}: {err}", info.index, info.device_name));
                    }
                }
            }

            bail!(
                "failed to start Windows Graphics Capture for active monitors: {}",
                last_error.unwrap_or_else(|| "no active monitors found".to_string())
            )
        }

        fn next_frame(&mut self) -> Result<CapturedVideoFrame> {
            self.rx
                .recv_timeout(self.timeout)
                .context("timed out waiting for Windows Graphics Capture frame")?
                .map_err(anyhow::Error::msg)
        }
    }

    impl Drop for WgcCaptureSource {
        fn drop(&mut self) {
            if let Some(control) = self.control.take() {
                let _ = control.stop();
            }
        }
    }

    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    enum CaptureBackend {
        Auto,
        Dxgi,
        Wgc,
    }

    fn capture_backend_from_env() -> Result<CaptureBackend> {
        match std::env::var("SUNRISE_CAPTURE_BACKEND") {
            Ok(value) => parse_capture_backend(&value),
            Err(std::env::VarError::NotPresent) => Ok(CaptureBackend::Auto),
            Err(err) => Err(anyhow::anyhow!(
                "failed to read SUNRISE_CAPTURE_BACKEND: {err}"
            )),
        }
    }

    fn parse_capture_backend(value: &str) -> Result<CaptureBackend> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(CaptureBackend::Auto),
            "dxgi" | "duplication" => Ok(CaptureBackend::Dxgi),
            "wgc" | "graphics-capture" | "windows-graphics-capture" => Ok(CaptureBackend::Wgc),
            value => {
                bail!("unsupported SUNRISE_CAPTURE_BACKEND={value:?}; expected auto, dxgi, or wgc")
            }
        }
    }

    struct DuplicationSource {
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

    fn create_duplication_source(monitor_index: Option<usize>) -> Result<DuplicationSource> {
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
                "probing DXGI duplication monitor"
            );
            match DxgiDuplicationApi::new(monitor) {
                Ok(duplication) => {
                    return Ok(DuplicationSource { duplication, info });
                }
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

    type WgcFrameResult = std::result::Result<CapturedVideoFrame, String>;

    struct WgcFrameCapture {
        tx: SyncSender<WgcFrameResult>,
        stop_after_first: bool,
    }

    impl GraphicsCaptureApiHandler for WgcFrameCapture {
        type Flags = (SyncSender<WgcFrameResult>, bool);
        type Error = String;

        fn new(ctx: CaptureContext<Self::Flags>) -> std::result::Result<Self, Self::Error> {
            Ok(Self {
                tx: ctx.flags.0,
                stop_after_first: ctx.flags.1,
            })
        }

        fn on_frame_arrived(
            &mut self,
            frame: &mut Frame,
            capture_control: InternalCaptureControl,
        ) -> std::result::Result<(), Self::Error> {
            let result = wgc_frame_to_captured_frame(frame).map_err(|err| err.to_string());
            let _ = self.tx.send(result);
            if self.stop_after_first {
                capture_control.stop();
            }
            Ok(())
        }

        fn on_closed(&mut self) -> std::result::Result<(), Self::Error> {
            let _ = self
                .tx
                .send(Err("Windows Graphics Capture item closed".to_string()));
            Ok(())
        }
    }

    fn capture_one_wgc_frame(monitor: Monitor, timeout_ms: u32) -> Result<CapturedVideoFrame> {
        let timeout = Duration::from_millis(u64::from(timeout_ms.max(1000)));
        let (control, rx) = start_wgc_capture(monitor, true)?;
        let frame = rx
            .recv_timeout(timeout)
            .context("timed out waiting for Windows Graphics Capture frame")?
            .map_err(anyhow::Error::msg);
        let stop_result = control.stop();
        if let Err(err) = stop_result {
            warn!(%err, "failed to stop Windows Graphics Capture cleanly");
        }
        frame
    }

    fn start_wgc_capture(
        monitor: Monitor,
        stop_after_first: bool,
    ) -> Result<(
        windows_capture::capture::CaptureControl<WgcFrameCapture, String>,
        Receiver<WgcFrameResult>,
    )> {
        let (tx, rx) = mpsc::sync_channel(8);
        let settings = Settings::new(
            monitor,
            CursorCaptureSettings::WithoutCursor,
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            (tx, stop_after_first),
        );
        let control = WgcFrameCapture::start_free_threaded(settings)
            .context("failed to start Windows Graphics Capture")?;
        Ok((control, rx))
    }

    fn wgc_frame_to_captured_frame(frame: &mut Frame) -> Result<CapturedVideoFrame> {
        let frame_index = frame
            .timestamp()
            .map(|timestamp| timestamp.Duration as u64)
            .unwrap_or(0);
        let frame_buffer = frame.buffer().context("failed to map WGC frame")?;
        let width = frame_buffer.width();
        let height = frame_buffer.height();
        let row_pitch = frame_buffer.row_pitch();
        let depth_pitch = frame_buffer.depth_pitch();
        let source_format = frame_buffer.color_format();
        let mut packed_storage = Vec::new();
        let packed_pixels = frame_buffer.as_nopadding_buffer(&mut packed_storage);
        let bgra = wgc_pixels_to_bgra8(width, height, source_format, packed_pixels)?;

        Ok(CapturedVideoFrame {
            width,
            height,
            stride: width * 4,
            row_pitch,
            depth_pitch,
            source_format: format!("WGC {source_format:?}"),
            frame_index,
            bgra,
        })
    }

    fn wgc_pixels_to_bgra8(
        width: u32,
        height: u32,
        format: ColorFormat,
        pixels: &[u8],
    ) -> Result<Vec<u8>> {
        let bytes_per_pixel = match format {
            ColorFormat::Bgra8 | ColorFormat::Rgba8 => 4,
            ColorFormat::Rgba16F => 8,
        };
        let pixel_len = width as usize * height as usize * bytes_per_pixel;
        let Some(pixels) = pixels.get(..pixel_len) else {
            bail!(
                "WGC buffer is too small: got {} bytes, expected {pixel_len}",
                pixels.len()
            );
        };

        Ok(match format {
            ColorFormat::Bgra8 => pixels.to_vec(),
            ColorFormat::Rgba8 => rgba_to_bgra(pixels),
            ColorFormat::Rgba16F => rgba16f_to_bgra8(pixels),
        })
    }

    fn capture_pixels_to_bgra8(
        width: u32,
        height: u32,
        format: DxgiDuplicationFormat,
        pixels: &[u8],
    ) -> Result<Vec<u8>> {
        let bytes_per_pixel = match format {
            DxgiDuplicationFormat::Bgra8
            | DxgiDuplicationFormat::Bgra8Srgb
            | DxgiDuplicationFormat::Rgba8
            | DxgiDuplicationFormat::Rgba8Srgb => 4,
            DxgiDuplicationFormat::Rgba16F => 8,
            DxgiDuplicationFormat::Rgb10A2 | DxgiDuplicationFormat::Rgb10XrA2 => {
                bail!(
                    "capture format {format:?} needs 10-bit conversion; this capture path currently supports 8-bit and Rgba16F"
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

        Ok(match format {
            DxgiDuplicationFormat::Bgra8 | DxgiDuplicationFormat::Bgra8Srgb => pixels.to_vec(),
            DxgiDuplicationFormat::Rgba8 | DxgiDuplicationFormat::Rgba8Srgb => rgba_to_bgra(pixels),
            DxgiDuplicationFormat::Rgba16F => rgba16f_to_bgra8(pixels),
            DxgiDuplicationFormat::Rgb10A2 | DxgiDuplicationFormat::Rgb10XrA2 => {
                unreachable!("10-bit formats bail above")
            }
        })
    }

    fn write_bgra_bmp(path: &Path, width: u32, height: u32, pixels: &[u8]) -> Result<usize> {
        let pixel_len = width as usize * height as usize * 4;
        let Some(pixels) = pixels.get(..pixel_len) else {
            bail!(
                "BGRA buffer is too small: got {} bytes, expected {pixel_len}",
                pixels.len()
            );
        };

        let header_len = 14_u32 + 40_u32;
        let file_size = header_len
            .checked_add(pixels.len() as u32)
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
        bmp.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
        bmp.extend_from_slice(&0_i32.to_le_bytes());
        bmp.extend_from_slice(&0_i32.to_le_bytes());
        bmp.extend_from_slice(&0_u32.to_le_bytes());
        bmp.extend_from_slice(&0_u32.to_le_bytes());
        bmp.extend_from_slice(pixels);

        fs::write(path, bmp)?;
        Ok(pixels.len())
    }

    fn rgba_to_bgra(pixels: &[u8]) -> Vec<u8> {
        let mut converted = pixels.to_vec();
        for pixel in converted.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        converted
    }

    fn rgba16f_to_bgra8(pixels: &[u8]) -> Vec<u8> {
        let mut converted = Vec::with_capacity(pixels.len() / 2);
        for pixel in pixels.chunks_exact(8) {
            let r = f16_to_f32(u16::from_le_bytes([pixel[0], pixel[1]]));
            let g = f16_to_f32(u16::from_le_bytes([pixel[2], pixel[3]]));
            let b = f16_to_f32(u16::from_le_bytes([pixel[4], pixel[5]]));
            let a = f16_to_f32(u16::from_le_bytes([pixel[6], pixel[7]]));
            converted.push(linear_float_to_srgb8(b));
            converted.push(linear_float_to_srgb8(g));
            converted.push(linear_float_to_srgb8(r));
            converted.push(float_alpha_to_u8(a));
        }
        converted
    }

    fn f16_to_f32(bits: u16) -> f32 {
        let sign = ((bits >> 15) & 0x1) as u32;
        let exponent = ((bits >> 10) & 0x1f) as i32;
        let fraction = (bits & 0x03ff) as u32;

        let f32_bits = if exponent == 0 {
            if fraction == 0 {
                sign << 31
            } else {
                let mut fraction = fraction;
                let mut exponent = -14_i32;
                while (fraction & 0x0400) == 0 {
                    fraction <<= 1;
                    exponent -= 1;
                }
                fraction &= 0x03ff;
                (sign << 31) | (((exponent + 127) as u32) << 23) | (fraction << 13)
            }
        } else if exponent == 0x1f {
            (sign << 31) | 0x7f80_0000 | (fraction << 13)
        } else {
            (sign << 31) | (((exponent - 15 + 127) as u32) << 23) | (fraction << 13)
        };

        f32::from_bits(f32_bits)
    }

    fn linear_float_to_srgb8(value: f32) -> u8 {
        if !value.is_finite() {
            return 0;
        }
        let value = value.clamp(0.0, 1.0);
        let encoded = if value <= 0.003_130_8 {
            value * 12.92
        } else {
            1.055 * value.powf(1.0 / 2.4) - 0.055
        };
        (encoded * 255.0).round().clamp(0.0, 255.0) as u8
    }

    fn float_alpha_to_u8(value: f32) -> u8 {
        if !value.is_finite() {
            return 255;
        }
        (value.clamp(0.0, 1.0) * 255.0).round().clamp(0.0, 255.0) as u8
    }

    #[cfg(test)]
    mod tests {
        use std::time::{SystemTime, UNIX_EPOCH};

        use anyhow::Result;

        use super::*;

        #[test]
        fn writes_top_down_bgra_bmp() -> Result<()> {
            let path = temp_bmp_path("bgra");
            let pixels = [1, 2, 3, 255, 4, 5, 6, 255];

            let bytes = write_bgra_bmp(&path, 2, 1, &pixels)?;
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
            let pixels = [10, 20, 30, 255];

            let converted = capture_pixels_to_bgra8(1, 1, DxgiDuplicationFormat::Rgba8, &pixels)?;

            assert_eq!(converted, [30, 20, 10, 255]);
            Ok(())
        }

        #[test]
        fn converts_rgba16f_bmp_pixels_to_bgra8() -> Result<()> {
            let pixels = [
                0x00, 0x3c, // R = 1.0
                0x00, 0x38, // G = 0.5
                0x00, 0x00, // B = 0.0
                0x00, 0x3c, // A = 1.0
            ];

            let converted = capture_pixels_to_bgra8(1, 1, DxgiDuplicationFormat::Rgba16F, &pixels)?;

            assert_eq!(converted, [0, 188, 255, 255]);
            Ok(())
        }

        #[test]
        fn converts_half_float_bits_to_f32() {
            assert_eq!(f16_to_f32(0x0000), 0.0);
            assert_eq!(f16_to_f32(0x3c00), 1.0);
            assert_eq!(f16_to_f32(0x3800), 0.5);
            assert_eq!(f16_to_f32(0xc000), -2.0);
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
