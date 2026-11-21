# Cross-platform PowerShell installer for Tessera (`tes.exe`) on Windows (x86_64 and ARM64).
$ErrorActionPreference = "Stop"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

function Write-Step($num, $msg) { Write-Host "`n[$num/3] $msg" -ForegroundColor Cyan }
function Write-Info($msg)       { Write-Host "  i $msg" -ForegroundColor Magenta }
function Write-Success($msg)    { Write-Host "  > $msg" -ForegroundColor Green }
function Write-Warn($msg)       { Write-Host "  ! $msg" -ForegroundColor Yellow }
function Write-ErrorMsg($msg)   { Write-Host "  X $msg" -ForegroundColor Red }

function Invoke-DownloadWithRetry {
    param(
        [Parameter(Mandatory = $true)][string]$Uri,
        [Parameter(Mandatory = $true)][string]$OutFile,
        [switch]$ShowProgress
    )

    $maxAttempts = 3
    $lastError = $null

    for ($attempt = 1; $attempt -le $maxAttempts; $attempt++) {
        if (Test-Path $OutFile) {
            Remove-Item -Force -Path $OutFile -ErrorAction SilentlyContinue
        }

        try {
            if (Get-Command curl.exe -ErrorAction SilentlyContinue) {
                if ($ShowProgress) {
                    & curl.exe -fL --progress-bar "$Uri" -o $OutFile
                } else {
                    & curl.exe -fsSL "$Uri" -o $OutFile
                }

                if ($LASTEXITCODE -ne 0) {
                    throw "curl.exe exited with code $LASTEXITCODE for $Uri"
                }
            } else {
                $prevProgress = $ProgressPreference
                try {
                    $ProgressPreference = if ($ShowProgress) { "Continue" } else { "SilentlyContinue" }
                    Invoke-WebRequest -Uri $Uri -OutFile $OutFile -UseBasicParsing
                } finally {
                    $ProgressPreference = $prevProgress
                }
            }

            if (-not (Test-Path $OutFile) -or ((Get-Item $OutFile).Length -eq 0)) {
                throw "Downloaded file is empty: $Uri"
            }

            return
        } catch {
            $lastError = $_
            if (Test-Path $OutFile) {
                Remove-Item -Force -Path $OutFile -ErrorAction SilentlyContinue
            }

            if ($attempt -lt $maxAttempts) {
                $delay = $attempt * 2
                Write-Warn "Download attempt $attempt/$maxAttempts failed ($($_.Exception.Message)); retrying in ${delay}s..."
                Start-Sleep -Seconds $delay
            }
        }
    }

    throw "Failed to download $Uri after $maxAttempts attempts: $($lastError.Exception.Message)"
}

$Repo = if ($env:TES_GITHUB_REPO) { $env:TES_GITHUB_REPO } else { "tessera-vcs/tessera" }
$Version = if ($env:TES_VERSION -and $env:TES_VERSION.Trim()) { $env:TES_VERSION.Trim() } else { "latest" }
if ($Version -ne "latest" -and -not $Version.StartsWith("v")) {
    $Version = "v$Version"
}

$InstallDir = if ($env:TES_INSTALL_DIR) { $env:TES_INSTALL_DIR } else { Join-Path $HOME ".local\bin" }

Write-Step "1" "Detecting platform & resolving release..."

$Arch = ""
try {
    $Arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
} catch {
    $Arch = ""
}

if (-not $Arch) {
    $procArch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    if ($procArch) {
        $Arch = $procArch.ToLowerInvariant()
    }
}

switch ($Arch) {
    "x64"   { $Target = "x86_64-pc-windows-msvc" }
    "amd64" { $Target = "x86_64-pc-windows-msvc" }
    "arm64" { $Target = "aarch64-pc-windows-msvc" }
    default {
        if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64" -or $env:PROCESSOR_ARCHITEW6432 -eq "ARM64") {
            $Target = "aarch64-pc-windows-msvc"
        } else {
            $Target = "x86_64-pc-windows-msvc"
        }
    }
}

$Archive = "tes-$Target.zip"

if ($env:TES_BASE_URL) {
    $BaseUrl = $env:TES_BASE_URL.TrimEnd("/")
    Write-Success "Target: $Target"
} elseif ($Version -eq "latest") {
    $BaseUrl = "https://github.com/$Repo/releases/latest/download"
    Write-Success "Target: $Target (latest)"
} else {
    $BaseUrl = "https://github.com/$Repo/releases/download/$Version"
    Write-Success "Target: $Target ($Version)"
}

$TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ("tes-install-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $TmpDir | Out-Null

try {
    $ZipPath = Join-Path $TmpDir $Archive
    $ShaPath = Join-Path $TmpDir "$Archive.sha256"

    Write-Step "2" "Downloading & verifying archive..."

    Write-Info "Downloading $Archive..."
    Invoke-DownloadWithRetry -Uri "$BaseUrl/$Archive" -OutFile $ZipPath -ShowProgress

    if ($env:TES_SKIP_CHECKSUM -ne "1") {
        Invoke-DownloadWithRetry -Uri "$BaseUrl/$Archive.sha256" -OutFile $ShaPath
    }

    Write-Success "Downloaded $Archive"

    if ($env:TES_SKIP_CHECKSUM -ne "1") {
        if (-not (Test-Path $ShaPath)) {
            throw "Missing SHA-256 checksum file for $Archive"
        }

        $ExpectedHash = ((Get-Content $ShaPath -Raw).Trim() -split '\s+')[0].ToLowerInvariant()
        if ($ExpectedHash -notmatch '^[0-9a-f]{64}$') {
            throw "Invalid SHA-256 checksum format in $Archive.sha256"
        }

        $ActualHash = (Get-FileHash -Path $ZipPath -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($ActualHash -ne $ExpectedHash) {
            throw "SHA-256 checksum mismatch for $Archive (expected $ExpectedHash, got $ActualHash)"
        }

        Write-Success "Verified SHA-256 checksum"
    }

    Write-Step "3" "Installing binary..."

    Expand-Archive -Path $ZipPath -DestinationPath $TmpDir -Force
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

    $DestBin = Join-Path $InstallDir "tes.exe"
    $StageBin = Join-Path $InstallDir "tes.exe.tmp"
    $OldBin = Join-Path $InstallDir "tes.exe.old"

    Copy-Item -Path (Join-Path $TmpDir "tes.exe") -Destination $StageBin -Force

    if (Test-Path $OldBin) {
        Remove-Item -Force -Path $OldBin -ErrorAction SilentlyContinue
    }

    try {
        if (Test-Path $DestBin) {
            try {
                Move-Item -Path $StageBin -Destination $DestBin -Force
            } catch {
                # Windows locks running executables against overwrite, but permits renaming them.
                $BackupBin = if (Test-Path $OldBin) { Join-Path $InstallDir "tes.exe.old.$PID" } else { $OldBin }
                try {
                    Move-Item -Path $DestBin -Destination $BackupBin -Force -ErrorAction Stop
                    Move-Item -Path $StageBin -Destination $DestBin -Force -ErrorAction Stop
                    Write-Warn "Existing tes.exe was in use; moved previous binary to $BackupBin"
                } catch {
                    throw "Could not replace $DestBin because it is locked by a running process. Close running 'tes' processes and re-run the installer. Details: $($_.Exception.Message)"
                }
            }
        } else {
            Move-Item -Path $StageBin -Destination $DestBin -Force
        }
    } finally {
        if (Test-Path $StageBin) {
            Remove-Item -Force -Path $StageBin -ErrorAction SilentlyContinue
        }
    }

    if (-not $env:TES_INSTALL_DIR) {
        $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
        if (-not $UserPath) {
            $UserPath = ""
        }

        $PathEntries = $UserPath -split ";" | Where-Object { $_ -ne "" }
        if ($PathEntries -notcontains $InstallDir) {
            $TrimmedUserPath = $UserPath.TrimEnd(";")
            $NewUserPath = if ($TrimmedUserPath) { "$TrimmedUserPath;$InstallDir" } else { $InstallDir }
            [Environment]::SetEnvironmentVariable("Path", $NewUserPath, "User")
            Write-Success "Added $InstallDir to user PATH"
        }
    }

    $CurrentPathEntries = if ($env:PATH) { ($env:PATH -split ";") | Where-Object { $_ -ne "" } } else { @() }
    if ($CurrentPathEntries -notcontains $InstallDir) {
        $TrimmedProcessPath = if ($env:PATH) { $env:PATH.TrimStart(";") } else { "" }
        $env:PATH = if ($TrimmedProcessPath) { "$InstallDir;$TrimmedProcessPath" } else { $InstallDir }
    }

    Write-Success "Installed tes to $DestBin"
    Write-Info "Run 'tes env --install' to configure PowerShell integration."
} finally {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
}
