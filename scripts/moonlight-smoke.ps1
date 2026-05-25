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

if (!(Test-Path -LiteralPath $MoonlightPath)) {
    throw "Moonlight.exe not found at $MoonlightPath"
}

$repoRoot = Resolve-Path "$PSScriptRoot\.."
$daemonPath = Join-Path $repoRoot "target\debug\sunrise-daemon.exe"
$logDir = Join-Path $repoRoot "target\moonlight-smoke"
$daemonLog = Join-Path $logDir "sunrise-daemon.log"
$pairLog = Join-Path $logDir "moonlight-pair.log"
$listLog = Join-Path $logDir "moonlight-list.log"

New-Item -ItemType Directory -Force -Path $logDir | Out-Null
Remove-Item -LiteralPath $daemonLog, $pairLog, $listLog -ErrorAction SilentlyContinue
New-SmokeConfig -Path $ConfigPath

Push-Location $repoRoot
try {
    cargo build -p sunrise-daemon
}
finally {
    Pop-Location
}

$daemonJob = Start-Job -ScriptBlock {
    param($ExePath, $ConfigPath, $Pin, $LogPath)
    $env:SUNRISE_PAIRING_PIN = $Pin
    & $ExePath --config $ConfigPath *>&1 | Tee-Object -FilePath $LogPath
} -ArgumentList $daemonPath, $ConfigPath, $Pin, $daemonLog

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
