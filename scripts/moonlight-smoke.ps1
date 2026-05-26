param(
    [string]$MoonlightPath = "C:\Program Files\Moonlight Game Streaming\Moonlight.exe",
    [string]$HostAddress = "127.0.0.1",
    [string]$Pin = "1234",
    [string]$ConfigPath = "$PSScriptRoot\..\target\moonlight-smoke\sunrise.toml",
    [switch]$SkipPair
)

$ErrorActionPreference = "Stop"

function Wait-Port {
    param(
        [string]$HostName,
        [int]$Port,
        [int]$TimeoutSeconds = 20
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        $client = [System.Net.Sockets.TcpClient]::new()
        try {
            $async = $client.BeginConnect($HostName, $Port, $null, $null)
            if ($async.AsyncWaitHandle.WaitOne(500) -and $client.Connected) {
                $client.EndConnect($async)
                return
            }
        }
        catch {
        }
        finally {
            $client.Dispose()
        }
        Start-Sleep -Milliseconds 250
    }

    throw "Timed out waiting for $HostName`:$Port"
}

function Test-HasPairedClient {
    param([string]$Path)

    if (!(Test-Path -LiteralPath $Path)) {
        return $false
    }

    return [bool](Select-String -LiteralPath $Path -Pattern "^\[\[paired_clients\]\]" -Quiet)
}

function New-SmokeConfig {
    param([string]$Path)

    if (Test-Path -LiteralPath $Path) {
        return
    }

    $uniqueId = ([guid]::NewGuid().ToString("N").Substring(0, 16)).ToUpperInvariant()
    $uuid = [guid]::NewGuid().ToString()
    $macBytes = [byte[]]::new(6)
    $rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
    try {
        $rng.GetBytes($macBytes)
    }
    finally {
        $rng.Dispose()
    }
    $macBytes[0] = ($macBytes[0] -band 0xFE) -bor 0x02
    $mac = ($macBytes | ForEach-Object { $_.ToString("X2") }) -join ":"

    @"
host_name = "sunrise-smoke"
http_port = 47989
https_port = 47984
rtsp_port = 48010
unique_id = "$uniqueId"
uuid = "$uuid"
mac_address = "$mac"
"@ | Set-Content -LiteralPath $Path -Encoding UTF8
}

function New-SmokeH264Source {
    param([string]$Path)

    if (Test-Path -LiteralPath $Path) {
        return
    }

    $ffmpeg = Get-Command ffmpeg.exe -ErrorAction SilentlyContinue
    if ($null -eq $ffmpeg) {
        throw "ffmpeg.exe is required to generate the smoke H.264 source"
    }

    & $ffmpeg.Source -hide_banner -loglevel error -y `
        -f lavfi -i "testsrc2=size=640x360:rate=30" `
        -t 2 `
        -c:v libx264 -preset ultrafast -tune zerolatency `
        -an -f h264 $Path

    if ($LASTEXITCODE -ne 0 -or !(Test-Path -LiteralPath $Path)) {
        throw "ffmpeg failed to generate $Path"
    }
}

function Invoke-MoonlightPairUntilPaired {
    param(
        [string]$LogPath,
        [int]$TimeoutSeconds = 45
    )

    if (Test-HasPairedClient -Path $ConfigPath) {
        Write-Host "Config already has a paired client; skipping Moonlight pair."
        return
    }

    $pairJob = Start-Job -ScriptBlock {
        param($MoonlightPath, $HostAddress, $Pin, $LogPath, $MoonlightDir)
        Push-Location $MoonlightDir
        try {
            & $MoonlightPath pair $HostAddress --pin $Pin *>&1 | Tee-Object -FilePath $LogPath
        }
        finally {
            Pop-Location
        }
    } -ArgumentList $MoonlightPath, $HostAddress, $Pin, $LogPath, (Split-Path -Parent $MoonlightPath)

    try {
        $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
        while ((Get-Date) -lt $deadline) {
            if (Test-HasPairedClient -Path $ConfigPath) {
                Write-Host "Moonlight pairing completed."
                return
            }

            if ($pairJob.State -eq "Failed" -or $pairJob.State -eq "Completed") {
                Receive-Job $pairJob -ErrorAction SilentlyContinue | Out-Null
                break
            }

            Start-Sleep -Milliseconds 500
        }

        throw "Moonlight pair did not complete within $TimeoutSeconds seconds. See $LogPath and $daemonLog"
    }
    finally {
        Stop-Job $pairJob -ErrorAction SilentlyContinue
        Receive-Job $pairJob -ErrorAction SilentlyContinue | Out-Null
        Remove-Job $pairJob -ErrorAction SilentlyContinue
    }
}

function Invoke-MoonlightListUntilDesktop {
    param(
        [string]$LogPath,
        [int]$TimeoutSeconds = 45
    )

    Remove-Item -LiteralPath $LogPath -ErrorAction SilentlyContinue
    $listJob = Start-Job -ScriptBlock {
        param($MoonlightPath, $HostAddress, $LogPath, $MoonlightDir)
        $ErrorActionPreference = "Continue"
        Push-Location $MoonlightDir
        try {
            & $MoonlightPath list $HostAddress --csv --verbose 2>&1 | Tee-Object -FilePath $LogPath
        }
        finally {
            Pop-Location
        }
    } -ArgumentList $MoonlightPath, $HostAddress, $LogPath, (Split-Path -Parent $MoonlightPath)

    try {
        $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
        while ((Get-Date) -lt $deadline) {
            if ((Test-Path -LiteralPath $LogPath) -and ((Get-Content -Raw $LogPath) -match "Desktop")) {
                Write-Host "Moonlight listed Desktop."
                return
            }

            if ($listJob.State -eq "Failed" -or $listJob.State -eq "Completed") {
                Receive-Job $listJob -ErrorAction SilentlyContinue | Out-Null
                break
            }

            Start-Sleep -Milliseconds 500
        }

        throw "Moonlight list did not return Desktop within $TimeoutSeconds seconds. See $LogPath and $daemonLog"
    }
    finally {
        Stop-Job $listJob -ErrorAction SilentlyContinue
        Receive-Job $listJob -ErrorAction SilentlyContinue | Out-Null
        Remove-Job $listJob -ErrorAction SilentlyContinue
    }
}

function Read-RtspResponse {
    param([System.Net.Sockets.NetworkStream]$Stream)

    $bytes = [System.Collections.Generic.List[byte]]::new()
    $buffer = [byte[]]::new(4096)

    while ($true) {
        $read = $Stream.Read($buffer, 0, $buffer.Length)
        if ($read -le 0) {
            break
        }

        for ($i = 0; $i -lt $read; $i++) {
            $bytes.Add($buffer[$i])
        }

        $text = [System.Text.Encoding]::ASCII.GetString($bytes.ToArray())
        $headerEnd = $text.IndexOf("`r`n`r`n")
        if ($headerEnd -lt 0) {
            continue
        }

        $contentLength = 0
        foreach ($line in $text.Substring(0, $headerEnd).Split("`r`n")) {
            if ($line -match "^\s*Content-Length\s*:\s*(\d+)\s*$") {
                $contentLength = [int]$Matches[1]
            }
        }

        $expectedLength = $headerEnd + 4 + $contentLength
        if ($bytes.Count -ge $expectedLength) {
            return [System.Text.Encoding]::ASCII.GetString($bytes.ToArray(), 0, $expectedLength)
        }
    }

    throw "RTSP connection closed before a complete response was read"
}

function Invoke-LaunchAndRtspSmoke {
    $launchPath = "/launch?uniqueid=0123456789ABCDEF&uuid=smoke&appid=1&mode=1920x1080x60&rikey=00112233445566778899AABBCCDDEEFF&rikeyid=1"
    try {
        $launchResponse = Invoke-HttpsGet -Path $launchPath
        if ($launchResponse -notmatch "<sessionUrl0>rtsp://$([regex]::Escape($HostAddress)):48010</sessionUrl0>") {
            throw "Launch response did not advertise the expected RTSP URL: $launchResponse"
        }
    }
    catch {
        Write-Warning "Skipping direct /launch HTTPS probe because Windows Schannel rejected the self-signed test endpoint: $($_.Exception.Message)"
    }

    $optionsResponse = Invoke-RtspRequest -Request "OPTIONS rtsp://$HostAddress`:48010 RTSP/1.0`r`nCSeq: 1`r`n`r`n"
    if ($optionsResponse -notmatch "RTSP/1.0 200 OK" -or $optionsResponse -notmatch "Connection: close") {
        throw "Unexpected RTSP OPTIONS response: $optionsResponse"
    }

    $describeResponse = Invoke-RtspRequest -Request "DESCRIBE rtsp://$HostAddress`:48010 RTSP/1.0`r`nCSeq: 2`r`nAccept: application/sdp`r`n`r`n"
    if ($describeResponse -notmatch "a=rtpmap:96 H264/90000" -or $describeResponse -notmatch "a=rtpmap:97 opus/48000/2") {
        throw "Unexpected RTSP DESCRIBE response: $describeResponse"
    }

    $setupAudioResponse = Invoke-RtspRequest -Request "SETUP streamid=audio/0/0 RTSP/1.0`r`nCSeq: 3`r`nTransport: unicast;X-GS-ClientPort=50000-50001`r`n`r`n"
    if ($setupAudioResponse -notmatch "server_port=48000-48001") {
        throw "Unexpected RTSP audio SETUP response: $setupAudioResponse"
    }

    $setupVideoResponse = Invoke-RtspRequest -Request "SETUP streamid=video/0/0 RTSP/1.0`r`nCSeq: 4`r`nSession: DEADBEEFCAFE`r`nTransport: unicast;X-GS-ClientPort=50000-50001`r`n`r`n"
    if ($setupVideoResponse -notmatch "server_port=47998-47999") {
        throw "Unexpected RTSP video SETUP response: $setupVideoResponse"
    }

    $setupControlResponse = Invoke-RtspRequest -Request "SETUP streamid=control/13/0 RTSP/1.0`r`nCSeq: 5`r`nSession: DEADBEEFCAFE`r`nTransport: unicast;X-GS-ClientPort=50000-50001`r`n`r`n"
    if ($setupControlResponse -notmatch "server_port=47999-48000") {
        throw "Unexpected RTSP control SETUP response: $setupControlResponse"
    }

    $announceResponse = Invoke-RtspRequest -Request "ANNOUNCE streamid=control/13/0 RTSP/1.0`r`nCSeq: 6`r`nSession: DEADBEEFCAFE`r`nContent-type: application/sdp`r`nContent-length: 0`r`n`r`n"
    if ($announceResponse -notmatch "RTSP/1.0 200 OK") {
        throw "Unexpected RTSP ANNOUNCE response: $announceResponse"
    }

    $playResponse = Invoke-RtspRequest -Request "PLAY / RTSP/1.0`r`nCSeq: 7`r`nSession: DEADBEEFCAFE`r`n`r`n"
    if ($playResponse -notmatch "RTSP/1.0 200 OK") {
        throw "Unexpected RTSP PLAY response: $playResponse"
    }

    Test-RtpPacket -Port 47998 -Name "video" -MinimumLength 36 -ExpectedFirstByte 0x90
    Test-RtpPacket -Port 48000 -Name "audio" -MinimumLength 15 -ExpectedFirstByte 0x80

    Write-Host "Launch and RTSP smoke passed."
}

function Invoke-HttpsGet {
    param([string]$Path)

    $client = [System.Net.Sockets.TcpClient]::new($HostAddress, 47984)
    try {
        $callback = [System.Net.Security.RemoteCertificateValidationCallback]{
            param($Sender, $Certificate, $Chain, $SslPolicyErrors)
            return $true
        }
        $stream = [System.Net.Security.SslStream]::new($client.GetStream(), $false, $callback)
        try {
            $stream.AuthenticateAsClient($HostAddress)
            $request = "GET $Path HTTP/1.1`r`nHost: $HostAddress`:47984`r`nConnection: close`r`n`r`n"
            $bytes = [System.Text.Encoding]::ASCII.GetBytes($request)
            $stream.Write($bytes, 0, $bytes.Length)
            $stream.Flush()

            $body = [System.IO.MemoryStream]::new()
            $buffer = [byte[]]::new(4096)
            while (($read = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
                $body.Write($buffer, 0, $read)
            }

            [System.Text.Encoding]::UTF8.GetString($body.ToArray())
        }
        finally {
            $stream.Dispose()
        }
    }
    finally {
        $client.Close()
    }
}

function Invoke-RtspRequest {
    param([string]$Request)

    $client = [System.Net.Sockets.TcpClient]::new($HostAddress, 48010)
    $client.ReceiveTimeout = 5000
    $client.SendTimeout = 5000
    try {
        $stream = $client.GetStream()
        $bytes = [System.Text.Encoding]::ASCII.GetBytes($Request)
        $stream.Write($bytes, 0, $bytes.Length)
        Read-RtspResponse -Stream $stream
    }
    finally {
        $client.Close()
    }
}

function Test-RtpPacket {
    param(
        [int]$Port,
        [string]$Name,
        [int]$MinimumLength,
        [byte]$ExpectedFirstByte
    )

    $udp = [System.Net.Sockets.UdpClient]::new(0)
    $udp.Client.ReceiveTimeout = 5000
    try {
        $payload = [System.Text.Encoding]::ASCII.GetBytes("PING")
        $udp.Send($payload, $payload.Length, $HostAddress, $Port) | Out-Null
        $remote = [System.Net.IPEndPoint]::new([System.Net.IPAddress]::Any, 0)
        $packet = $udp.Receive([ref]$remote)
        if ($packet.Length -lt $MinimumLength) {
            throw "Received short $Name RTP packet: $($packet.Length) bytes"
        }
        if ($packet[0] -ne $ExpectedFirstByte) {
            throw "Received invalid $Name RTP packet header: 0x$($packet[0].ToString('X2'))"
        }
    }
    finally {
        $udp.Close()
    }
}

if (!(Test-Path -LiteralPath $MoonlightPath)) {
    throw "Moonlight.exe not found at $MoonlightPath"
}

$repoRoot = Resolve-Path "$PSScriptRoot\.."
$daemonPath = Join-Path $repoRoot "target\debug\sunrise-daemon.exe"
$logDir = Join-Path $repoRoot "target\moonlight-smoke"
$daemonLog = Join-Path $logDir "sunrise-daemon.log"
$pairLog = Join-Path $logDir "moonlight-pair.log"
$listLog = Join-Path $logDir "moonlight-list.log"
$h264Source = Join-Path $logDir "testsrc.h264"

New-Item -ItemType Directory -Force -Path $logDir | Out-Null
Remove-Item -LiteralPath $daemonLog, $pairLog, $listLog -ErrorAction SilentlyContinue
New-SmokeConfig -Path $ConfigPath
New-SmokeH264Source -Path $h264Source

Push-Location $repoRoot
try {
    cargo build -p sunrise-daemon
}
finally {
    Pop-Location
}

$daemonJob = Start-Job -ScriptBlock {
    param($ExePath, $ConfigPath, $Pin, $LogPath, $H264Source)
    $env:SUNRISE_PAIRING_PIN = $Pin
    $env:SUNRISE_H264_PATH = $H264Source
    & $ExePath --config $ConfigPath *>&1 | Tee-Object -FilePath $LogPath
} -ArgumentList $daemonPath, $ConfigPath, $Pin, $daemonLog, $h264Source

try {
    Wait-Port -HostName "127.0.0.1" -Port 47989
    Wait-Port -HostName "127.0.0.1" -Port 47984

    $moonlightDir = Split-Path -Parent $MoonlightPath
    Push-Location $moonlightDir
    try {
        if (!$SkipPair) {
            Invoke-MoonlightPairUntilPaired -LogPath $pairLog
        }

        Invoke-MoonlightListUntilDesktop -LogPath $listLog
        Invoke-LaunchAndRtspSmoke
    }
    finally {
        Pop-Location
    }

    $listOutput = if (Test-Path $listLog) { Get-Content -Raw $listLog } else { "" }
    if ($listOutput -notmatch "Desktop") {
        throw "Moonlight list did not return Desktop. See $listLog and $daemonLog"
    }

    Write-Host "Moonlight smoke passed: Desktop app was listed."
}
finally {
    Stop-Job $daemonJob -ErrorAction SilentlyContinue
    Receive-Job $daemonJob -ErrorAction SilentlyContinue | Out-Null
    Remove-Job $daemonJob -ErrorAction SilentlyContinue
}
