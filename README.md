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

The script builds sunrise, starts the daemon with a test PIN, runs the real Moonlight CLI to pair with `127.0.0.1`, then runs `Moonlight.exe list 127.0.0.1 --csv --verbose` and checks that `Desktop` is returned. It also probes `/launch`, sends persistent RTSP `OPTIONS`, `DESCRIBE`, `SETUP`, and `PLAY` requests over one TCP connection, sends UDP pings, and verifies that video/audio RTP packets are returned.

The smoke config uses a dedicated `sunrise-smoke` host name so Moonlight's local certificate cache does not collide with real hosts or previous experiments using the Windows computer name.

The smoke test uses local `ffmpeg.exe` to generate `target\moonlight-smoke\testsrc.h264`. To use your own Annex B H.264 elementary stream when running the daemon manually:

```powershell
$env:SUNRISE_H264_PATH = "C:\path\to\sample.h264"
cargo run -p sunrise-daemon
```

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

After `SETUP` and `PLAY`, sunrise binds the advertised UDP ports, waits for the client's UDP ping, then sends RTP packets. Video packets are sourced from an Annex B H.264 elementary stream via `SUNRISE_H264_PATH`. Audio currently sends minimal Opus silence packets.

## Current Limitations

- Client certificate signature verification is not implemented.
- `/launch` is a session skeleton and does not start a real desktop capture pipeline.
- RTP video is a file-backed H.264 simulator, not a live desktop stream.
- RTP audio is an unencrypted Opus-silence placeholder; real encrypted Opus audio is not implemented.
- ENet control/input is not implemented.
- No video capture, audio capture, NVENC, or Windows screen capture exists yet.
- The XML is plausible and easy to tweak, but may need field/value adjustments after testing against real Moonlight versions.

## Next Milestones

1. Verify the `/launch` and RTSP skeleton against more Moonlight clients.
2. Gate HTTPS APIs by paired client certificates.
3. Add ENet control channel parsing.
4. Add RTP video and audio transport scaffolding.
5. Add Windows capture/encode integration only after the protocol control plane is stable.
