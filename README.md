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
cargo run -p sunrise-daemon --features capture-windows -- capture-smoke --output target\capture-smoke\frame.bmp
```

The capture source can also run a short continuous loop and report the observed capture throughput:

```powershell
cargo run -p sunrise-daemon --features capture-windows -- capture-loop --frames 120
```

This supports 8-bit BGRA/RGBA surfaces and converts HDR-style `Rgba16F` desktop frames to SDR BGRA for the current CPU-side frame boundary. It must run in an interactive Windows desktop session. If the capture API returns `Access denied`, rerun from a normal/elevated terminal outside restricted sandboxes. These commands validate frame acquisition; the source is not yet wired into RTSP video or NVENC.

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

This keeps the file-backed H.264 source as a test source while leaving a clean place to plug in live Windows capture plus NVENC output later. The RTSP control `SETUP` also starts a minimal ENet listener on UDP `47999` so Moonlight can establish the control stream transport.

## Current Limitations

- Client certificate signature verification is not implemented.
- `/launch` is a session skeleton and does not start a real desktop capture pipeline.
- RTP video is driven through the media framework, but the only implemented video source is still a file-backed H.264 simulator.
- Running without `SUNRISE_H264_PATH` uses a tiny fallback placeholder and may show a black screen.
- Windows capture has a DXGI frame source and smoke/loop tests, but live capture is not connected to the media pipeline yet.
- RTP audio is an unencrypted Opus-silence placeholder; real encrypted Opus audio is not implemented.
- ENet control accepts connections and logs packets, but real AES-GCM GameStream control message handling and input injection are not implemented.
- No live video capture loop, audio capture, or NVENC implementation exists yet.
- The XML is plausible and easy to tweak, but may need field/value adjustments after testing against real Moonlight versions.

## Next Milestones

1. Verify the `/launch` and RTSP skeleton against more Moonlight clients.
2. Gate HTTPS APIs by paired client certificates.
3. Add GameStream ENet control message parsing and required control replies.
4. Promote the one-frame Windows capture smoke path into a live frame source.
5. Replace the file-backed `AnnexBVideoSource` with a live encoded source fed by Windows capture plus NVENC.
6. Add real audio capture, Opus encoding, and GameStream audio encryption.
