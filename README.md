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

The script builds sunrise, starts the daemon with a test PIN, runs the real Moonlight CLI to pair with `127.0.0.1`, then runs `Moonlight.exe list 127.0.0.1 --csv --verbose` and checks that `Desktop` is returned.

The smoke config uses a dedicated `sunrise-smoke` host name so Moonlight's local certificate cache does not collide with real hosts or previous experiments using the Windows computer name.

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

This is only a handshake scaffold. It advertises placeholder H.264 video and Opus audio SDP, but it does not send RTP video/audio packets yet.

## Current Limitations

- Client certificate signature verification is not implemented.
- `/launch` is a session skeleton and does not start a real desktop capture pipeline.
- RTSP replies are placeholders for Moonlight probing and early negotiation.
- RTP video and audio are not implemented.
- ENet control/input is not implemented.
- No video capture, audio capture, NVENC, or Windows screen capture exists yet.
- The XML is plausible and easy to tweak, but may need field/value adjustments after testing against real Moonlight versions.

## Next Milestones

1. Verify the `/launch` and RTSP skeleton against more Moonlight clients.
2. Gate HTTPS APIs by paired client certificates.
3. Add ENet control channel parsing.
4. Add RTP video and audio transport scaffolding.
5. Add Windows capture/encode integration only after the protocol control plane is stable.
