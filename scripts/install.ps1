# Installs or updates Tworky on Windows from the latest GitHub release.
#
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/tristanmenzel/Tworky/main/scripts/install.ps1)))
#
# It never calls `exit`, which would close the shell that ran the one-liner.
#
# Downloading with Invoke-WebRequest sets no Mark-of-the-Web, so SmartScreen never sees the unsigned
# installer.
#
# The app runs it with -WaitPid and -StatusDir: it downloads while the app keeps running, reports
# progress in <StatusDir>/status.json, then installs once the app has exited and reopens it if the
# app left a `relaunch` file.
param(
    [string]$Repo = 'tristanmenzel/Tworky',
    [int]$WaitPid = 0,
    [string]$StatusDir = ''
)

$ErrorActionPreference = 'Stop'
$fromApp = $WaitPid -ne 0 -and $StatusDir -ne ''
$logFile = if ($StatusDir) { Join-Path $StatusDir 'update.log' } else { $null }

function Write-Log([string]$line) {
    Write-Host $line
    if ($logFile) { Add-Content -LiteralPath $logFile -Value $line -Encoding utf8 }
}

function Set-Phase([string]$phase, [string]$message = '') {
    if (-not $StatusDir) { return }
    $json = @{ phase = $phase; message = $message } | ConvertTo-Json -Compress
    $tmp = Join-Path $StatusDir 'status.json.tmp'
    [System.IO.File]::WriteAllText($tmp, $json)
    Move-Item -LiteralPath $tmp -Destination (Join-Path $StatusDir 'status.json') -Force
}

function Get-InstalledExe {
    $key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Tworky'
    $icon = (Get-ItemProperty -LiteralPath $key -ErrorAction SilentlyContinue).DisplayIcon
    if ($icon) { return $icon.Trim('"') }
    $fallback = Join-Path $env:LOCALAPPDATA 'Tworky\tworky.exe'
    if (Test-Path -LiteralPath $fallback) { return $fallback }
    return $null
}

function Start-Tworky {
    $exe = Get-InstalledExe
    if ($exe) {
        Write-Log "Starting $exe"
        Start-Process -FilePath $exe
    }
}

function Wait-ForApp {
    if ($WaitPid) {
        Write-Log 'Waiting for Tworky to close'
        while (Get-Process -Id $WaitPid -ErrorAction SilentlyContinue) { Start-Sleep -Milliseconds 500 }
        return
    }
    if (Get-Process -Name tworky -ErrorAction SilentlyContinue) {
        Write-Log 'Tworky is running. Close it to continue; the update installs once it has exited.'
        while (Get-Process -Name tworky -ErrorAction SilentlyContinue) { Start-Sleep -Milliseconds 500 }
    }
}

$installed = $false
try {
    Set-Phase 'downloading'
    $protocols = [Net.ServicePointManager]::SecurityProtocol
    if ($protocols -ne 0 -and -not ($protocols -band [Net.SecurityProtocolType]::Tls12)) {
        # Windows PowerShell 5.1 on older Windows offers only TLS 1.0, which GitHub refuses.
        [Net.ServicePointManager]::SecurityProtocol = $protocols -bor [Net.SecurityProtocolType]::Tls12
    }
    $ProgressPreference = 'SilentlyContinue'

    # The API doesn't list releases in publish order. Drafts are hidden from unauthenticated callers.
    $releases = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases?per_page=20" -UseBasicParsing
    $asset = $releases | Sort-Object { [datetime]$_.published_at } -Descending |
        ForEach-Object { $_.assets } |
        Where-Object { $_.name -like '*_x64-setup.exe' } | Select-Object -First 1
    if (-not $asset) { throw 'No recent release has a Windows installer.' }

    $download = Join-Path ([System.IO.Path]::GetTempPath()) ("tworky-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $download | Out-Null
    $installerPath = Join-Path $download $asset.name
    Write-Log "Downloading $($asset.browser_download_url)"
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $installerPath -UseBasicParsing
    $installer = Get-Item -LiteralPath $installerPath

    Set-Phase 'ready'
    Wait-ForApp

    Set-Phase 'installing'
    Write-Log "Installing $($installer.Name)"
    $proc = Start-Process -FilePath $installer.FullName -ArgumentList '/S' -Wait -PassThru
    if ($proc.ExitCode -ne 0) { throw "The installer failed (exit $($proc.ExitCode))" }
    $installed = $true
    Remove-Item -LiteralPath $download -Recurse -Force -ErrorAction SilentlyContinue
    Set-Phase 'done'
    Write-Log 'Tworky is installed.'
} catch {
    Write-Log "Update failed: $($_.Exception.Message)"
    Set-Phase 'failed' $_.Exception.Message
} finally {
    $relaunch = -not $fromApp -or (Test-Path -LiteralPath (Join-Path $StatusDir 'relaunch'))
    if ($fromApp) { Remove-Item -LiteralPath (Join-Path $StatusDir 'relaunch') -ErrorAction SilentlyContinue }
    $appClosed = -not $WaitPid -or -not (Get-Process -Id $WaitPid -ErrorAction SilentlyContinue)
    if ($relaunch -and $appClosed -and ($installed -or $fromApp)) { Start-Tworky }
}
