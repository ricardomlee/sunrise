# sunrise

sunrise is a clean-room Rust foundation for a minimal Sunshine-compatible / NVIDIA GameStream-compatible host that Moonlight clients can probe.

This is intentionally not a full Sunshine replacement. The current milestone only exposes enough HTTP and HTTPS surface area for early Moonlight discovery experiments.

## Build

```powershell
cargo build
```

## Run

```powershell
cargo run -p sunrise-daemon
```

On first run, sunrise creates `sunrise.toml` in the current directory. To use another path:

```powershell
cargo run -p sunrise-daemon -- --config path\to\sunrise.toml
```

The daemon listens on:

- HTTP: `0.0.0.0:47989`
- HTTPS: `0.0.0.0:47984`
- RTSP: `0.0.0.0:48010`

If Windows reports `os error 10048` on one of the GameStream ports but `netstat` and
`Get-NetTCPConnection` do not show an owner, bind sunrise to the LAN address that
Moonlight will connect to:

```powershell
$env:SUNRISE_BIND_IP = "192.168.2.123"
cargo run -p sunrise-daemon --features native-nvenc
```

This keeps the standard Moonlight ports while avoiding wildcard `0.0.0.0` bind
conflicts from hidden reservations or stale listeners.

## Test Locally

Open:

```text
http://127.0.0.1:47989/serverinfo
```

You should see XML with host identity and GameStream-like fields.

The HTTPS `/applist` endpoint uses a self-signed certificate, so command-line clients need certificate validation disabled:

```powershell
curl.exe -k https://127.0.0.1:47984/applist
```

It returns one fake app:

- ID: `1`
- AppTitle: `Desktop`
- IsHdrSupported: `0`

## Automated Moonlight Smoke Test

On Windows with Moonlight installed:

```powershell
.\scripts\moonlight-smoke.ps1
```

The script builds sunrise, starts the daemon with a test PIN, runs the real Moonlight CLI to pair with `127.0.0.1`, then runs `Moonlight.exe list 127.0.0.1 --csv --verbose` and checks that `Desktop` is returned. It also probes `/launch`, sends RTSP `OPTIONS`, `DESCRIBE`, `SETUP`, and `PLAY` requests using Moonlight-compatible close-after-response TCP transactions, sends UDP pings, and verifies that video/audio RTP packets are returned.

The smoke config uses a dedicated `sunrise-smoke` host name so Moonlight's local certificate cache does not collide with real hosts or previous experiments using the Windows computer name.

The smoke test uses local `ffmpeg.exe` to generate `target\moonlight-smoke\testsrc.h264`. To use your own Annex B H.264 elementary stream when running the daemon manually:

```powershell
$env:SUNRISE_H264_PATH = "C:\path\to\sample.h264"
cargo run -p sunrise-daemon
```

## Windows Capture Smoke Test

The first native Windows capture path is available behind the `capture-windows` feature. It uses `windows-capture` with DXGI Desktop Duplication to grab one monitor frame and writes a 32-bit BGRA BMP:

```powershell
cargo run -p sunrise-daemon --features capture-windows -- capture-list
```

`capture-list` prints each active monitor, its Windows display device, adapter string, resolution, refresh rate, and whether a DXGI duplication session can be created. This is useful on headless machines with virtual displays such as Parsec VDD. When no monitor is specified, sunrise probes the primary monitor first and then the remaining active monitors until one accepts DXGI duplication.

```powershell
cargo run -p sunrise-daemon --features capture-windows -- capture-smoke --output target\capture-smoke\frame.bmp
```

If DXGI Desktop Duplication rejects a virtual or headless display with `Access denied`, try the Windows Graphics Capture smoke path against the same monitor:

```powershell
cargo run -p sunrise-daemon --features capture-windows -- wgc-smoke --monitor 17 --output target\capture-smoke\wgc-frame.bmp
```

This uses the Windows Graphics Capture API instead of DXGI output duplication and is the intended fallback candidate for Parsec VDD and other virtual displays.

The capture source can also run a short continuous loop and report the observed capture throughput:

```powershell
cargo run -p sunrise-daemon --features capture-windows -- capture-loop --frames 120
```

To validate the next boundary, capture frames can be encoded to an Annex B H.264 elementary stream with ffmpeg. The default encoder is `auto`, which tries `h264_nvenc` first and falls back to `libx264` if NVENC is unavailable:

```powershell
cargo run -p sunrise-daemon --features capture-windows -- encode-smoke --frames 120 --fps 30 --output target\capture-smoke\capture.h264
```

For Intel integrated graphics, `qsv-smoke` validates the capture-to-H.264 path through FFmpeg's `h264_qsv` encoder:

```powershell
cargo run -p sunrise-daemon --features capture-windows -- qsv-smoke --frames 120 --fps 30 --output target\capture-smoke\qsv.h264
```

This supports 8-bit BGRA/RGBA surfaces and converts HDR-style `Rgba16F` desktop frames to SDR BGRA for the current CPU-side frame boundary. The QSV smoke path converts BGRA to NV12 and uploads frames into a QSV hardware device before encoding. It must run in an interactive Windows desktop session. If the capture API returns `Access denied`, rerun from a normal/elevated terminal outside restricted sandboxes.

For the native no-ffmpeg path, `native-nvenc-smoke` captures a DXGI D3D11 texture and registers a D3D11 texture directly with NVENC:

```powershell
cargo run -p sunrise-daemon --features native-nvenc -- native-nvenc-smoke --frames 120 --fps 30 --output target\capture-smoke\native-nvenc.h264
```

If the desktop is exposed as HDR/scRGB (`Rgba16F`) or 10-bit RGB, sunrise performs a GPU render pass into a BGRA8 D3D11 texture before NVENC registration. This keeps the native path on the GPU side; it does not pipe raw frames through ffmpeg or a CPU raw-video boundary. This command requires an NVIDIA driver that exposes `nvEncodeAPI64.dll`.

DXGI Desktop Duplication only produces a new frame when the desktop changes. During the native NVENC smoke test, sunrise keeps a persistent GPU input texture and reuses the last captured frame when `AcquireNextFrame` times out, so a static desktop can still produce the requested number of encoded frames.

To make RTSP/RTP use live capture and native NVENC instead of `SUNRISE_H264_PATH`, run the daemon with the native feature and opt in explicitly:

```powershell
$env:SUNRISE_VIDEO_SOURCE = "native-nvenc"
cargo run -p sunrise-daemon --features native-nvenc
```

When the `native-nvenc` feature is compiled in, the daemon defaults to live capture unless `SUNRISE_VIDEO_SOURCE` is explicitly set to `annex-b`, `file`, or `h264`. This prevents a stale `SUNRISE_H264_PATH` from silently masking capture failures during live testing.

Optional knobs:

```powershell
$env:SUNRISE_VIDEO_FPS = "30"
$env:SUNRISE_CAPTURE_MONITOR = "1"
$env:SUNRISE_CAPTURE_TIMEOUT_MS = "33"
```

For Intel QSV live testing, build with the capture feature and select QSV explicitly:

```powershell
$env:SUNRISE_VIDEO_SOURCE = "qsv"
cargo run -p sunrise-daemon --features capture-windows
```

The current QSV live source is an FFmpeg-backed bridge: Rust captures frames, feeds `h264_qsv`, reads Annex B H.264 from stdout, and reuses the normal RTP packetizer. A later native oneVPL/D3D11 source should remove this subprocess boundary.

On headless systems, run `capture-list` first. If the Parsec VDD output is not the primary monitor, set `SUNRISE_CAPTURE_MONITOR` to that one-based monitor index. If the VDD is exposed through a non-NVIDIA adapter, DXGI capture may still work but zero-copy NVENC may need a later cross-adapter copy path.

If your Moonlight install is in a different location:

```powershell
.\scripts\moonlight-smoke.ps1 -MoonlightPath "C:\Path\To\Moonlight.exe"
```

## Test With Moonlight

1. Start sunrise:

   ```powershell
   cargo run -p sunrise-daemon
   ```

2. Find the Windows host IP address on your LAN.
3. In Moonlight, manually add that IP address.
4. Start pairing from Moonlight.
5. When sunrise prints a terminal prompt, type the PIN shown by Moonlight and press Enter.
6. Watch the sunrise logs for `/serverinfo`, `/pair`, and `/applist` requests.

The current pairing implementation is intentionally early-stage. It performs the PIN-derived AES challenge exchange and persists paired client certificates. Client certificate signature verification is still TODO.

## Launch And RTSP Skeleton

sunrise now accepts HTTPS `/launch` for the fake `Desktop` app and returns a plain RTSP session URL. It also starts a TCP RTSP listener on port `48010` that can answer the first control-plane requests Moonlight sends when a stream starts:

- `OPTIONS`
- `DESCRIBE`
- `SETUP`
- `ANNOUNCE`
- `PLAY`
- `GET_PARAMETER`
- `TEARDOWN`

After `SETUP` and `PLAY`, sunrise binds the advertised UDP ports, waits for the client's UDP ping, then sends RTP packets. The RTSP layer now delegates media production to a small framework:

- `AnnexBVideoSource` reads an Annex B H.264 elementary stream from `SUNRISE_H264_PATH`, groups NAL units into access units, and exposes encoded frames.
- `VideoPacketizer` emits Moonlight-style video RTP packets with the RTP extension flag, little-endian NV video headers, stream packet indices, and frame packet metadata.
- `OpusSilenceSource` and `AudioPacketizer` provide the temporary audio path.

For file-backed smoke testing with a native build, select the file source explicitly:

```powershell
$env:SUNRISE_VIDEO_SOURCE = "annex-b"
$env:SUNRISE_H264_PATH = "C:\path\to\sample.h264"
```

This keeps the file-backed H.264 source as an explicit test source while leaving a clean place for live Windows capture plus NVENC output. The RTSP control `SETUP` also starts a minimal ENet listener on UDP `47999` so Moonlight can establish the control stream transport.

## Current Limitations

- Client certificate signature verification is not implemented.
- `/launch` is a session skeleton and does not start a real desktop capture pipeline.
- RTP video is driven through the media framework. Native builds default to live D3D11 NVENC capture; file-backed H.264 is still available with `SUNRISE_VIDEO_SOURCE=annex-b`.
- Running the file source without `SUNRISE_H264_PATH` uses a tiny fallback placeholder and may show a black screen.
- Windows capture has a DXGI frame source plus smoke/loop tests.
- H.264 encode smoke can produce Annex B output from captured frames through ffmpeg, including `h264_qsv` for Intel hardware validation.
- RTSP video can explicitly use an FFmpeg-backed QSV live source with `SUNRISE_VIDEO_SOURCE=qsv`.
- Native D3D11 NVENC can register captured textures directly with NVENC and uses a GPU BGRA conversion pass for HDR/10-bit desktop frames. It still needs more testing on NVIDIA headless and virtual-display hosts.
- RTP audio is an unencrypted Opus-silence placeholder; real encrypted Opus audio is not implemented.
- ENet control accepts connections and logs packets, but real AES-GCM GameStream control message handling and input injection are not implemented.
- No live video capture-to-RTSP loop or audio capture implementation exists yet.
- The XML is plausible and easy to tweak, but may need field/value adjustments after testing against real Moonlight versions.

## Next Milestones

1. Verify the `/launch` and RTSP skeleton against more Moonlight clients.
2. Gate HTTPS APIs by paired client certificates.
3. Add GameStream ENet control message parsing and required control replies.
4. Harden live capture/NVENC across headless, virtual-display, and multi-adapter systems.
5. Add real audio capture, Opus encoding, and GameStream audio encryption.
6. Fill out GameStream control messages and input injection.
