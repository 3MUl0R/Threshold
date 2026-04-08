# Threshold Daemon Wrapper for Windows
# Keeps the daemon running and handles restart coordination.
# Equivalent of scripts/threshold-wrapper.sh for macOS/Linux.

param(
    [string]$DataDir = "",
    [string]$Config = ""
)

$ErrorActionPreference = "Stop"

# Resolve data directory
if ($DataDir -eq "") {
    $DataDir = if ($env:THRESHOLD_DATA_DIR) { $env:THRESHOLD_DATA_DIR } else { Join-Path $HOME ".threshold" }
}
if ($Config -eq "") {
    $Config = if ($env:THRESHOLD_CONFIG) { $env:THRESHOLD_CONFIG } else { "" }
}

$StateDir = Join-Path $DataDir "state"
$SupervisedMarker = Join-Path $StateDir "supervised"
$StopSentinel = Join-Path $StateDir "stop-sentinel"
$RestartPending = Join-Path $StateDir "restart-pending.json"

# Find repo root (parent of scripts/)
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Split-Path -Parent $ScriptDir

# Find threshold binary
$Binary = Join-Path (Join-Path (Join-Path $RepoRoot "target") "release") "threshold.exe"
if (-not (Test-Path $Binary)) {
    $Binary = Join-Path (Join-Path (Join-Path $RepoRoot "target") "debug") "threshold.exe"
}

# Ensure state directory exists
if (-not (Test-Path $StateDir)) {
    New-Item -ItemType Directory -Path $StateDir -Force | Out-Null
}

function Write-SupervisedMarker {
    $marker = @{
        wrapper_pid = $PID
        started_at  = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    } | ConvertTo-Json -Compress
    Set-Content -Path $SupervisedMarker -Value $marker -NoNewline
}

function Remove-SupervisedMarker {
    if (Test-Path $SupervisedMarker) {
        Remove-Item -Path $SupervisedMarker -Force -ErrorAction SilentlyContinue
    }
}

function Test-StopSentinel {
    return (Test-Path $StopSentinel)
}

function Remove-StopSentinel {
    if (Test-Path $StopSentinel) {
        Remove-Item -Path $StopSentinel -Force -ErrorAction SilentlyContinue
    }
}

function Build-Threshold {
    Write-Host "Building threshold..."
    Push-Location $RepoRoot
    try {
        & cargo build --release -p threshold 2>&1
        if ($LASTEXITCODE -ne 0) {
            Write-Host "Build failed (exit code $LASTEXITCODE). Continuing with existing binary."
            return $false
        }
        $script:Binary = Join-Path (Join-Path (Join-Path $RepoRoot "target") "release") "threshold.exe"
        Write-Host "Build succeeded."
        return $true
    } finally {
        Pop-Location
    }
}

# Initial build if no binary exists
if (-not (Test-Path $Binary)) {
    Write-Host "No threshold binary found. Building..."
    if (-not (Build-Threshold)) {
        Write-Host "Initial build failed and no existing binary. Exiting."
        exit 1
    }
}

# Clean up any stale stop sentinel from previous runs
Remove-StopSentinel

# Write supervised marker
Write-SupervisedMarker

try {
    Write-Host "Threshold wrapper started (PID: $PID)"
    Write-Host "Data dir: $DataDir"
    Write-Host "Binary: $Binary"

    while ($true) {
        # Check stop sentinel before starting
        if (Test-StopSentinel) {
            Write-Host "Stop sentinel detected. Exiting wrapper."
            Remove-StopSentinel
            break
        }

        # Check for restart-pending (rebuild request)
        if (Test-Path $RestartPending) {
            try {
                $pending = Get-Content $RestartPending -Raw | ConvertFrom-Json
                if (-not $pending.skip_build) {
                    Build-Threshold | Out-Null
                }
            } catch {
                Write-Host "Warning: Could not parse restart-pending.json: $_"
            }
            Remove-Item -Path $RestartPending -Force -ErrorAction SilentlyContinue
        }

        # Build daemon command arguments
        $args = @("daemon", "start")
        if ($DataDir) {
            $args += @("--data-dir", $DataDir)
        }
        if ($Config) {
            $args += @("--config", $Config)
        }

        # Run the daemon
        Write-Host "Starting daemon: $Binary $($args -join ' ')"
        $process = Start-Process -FilePath $Binary -ArgumentList $args -NoNewWindow -PassThru -Wait

        $exitCode = $process.ExitCode
        Write-Host "Daemon exited with code $exitCode"

        # Check stop sentinel after daemon exits
        if (Test-StopSentinel) {
            Write-Host "Stop sentinel detected after daemon exit. Exiting wrapper."
            Remove-StopSentinel
            break
        }

        # If non-zero exit (crash), wait before restarting
        if ($exitCode -ne 0) {
            Write-Host "Daemon crashed. Waiting 5 seconds before restart..."
            Start-Sleep -Seconds 5
        }

        # Update supervised marker for new loop iteration
        Write-SupervisedMarker
    }
} finally {
    Remove-SupervisedMarker
    Write-Host "Threshold wrapper exiting."
}

exit 0
