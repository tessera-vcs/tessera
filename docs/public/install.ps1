# Cross-platform PowerShell installer for Tessera (`tes.exe`) on Windows (x86_64 and ARM64).
$ErrorActionPreference = "Stop"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

function Write-Step($num, $msg) { Write-Host "`n[$num/3] $msg" -ForegroundColor Cyan }
function Write-Info($msg)       { Write-Host "  i $msg" -ForegroundColor Magenta }
function Write-Success($msg)    { Write-Host "  > $msg" -ForegroundColor Green }
function Write-Warn($msg)       { Write-Host "  ! $msg" -ForegroundColor Yellow }
function Write-ErrorMsg($msg)   { Write-Host "  X $msg" -ForegroundColor Red }

$Repo = if ($env:TES_GITHUB_REPO) { $env:TES_GITHUB_REPO } else { "tessera-vcs/tessera" }
$Version = if ($env:TES_VERSION) { $env:TES_VERSION } else { "latest" }
$InstallDir = if ($env:TES_INSTALL_DIR) { $env:TES_INSTALL_DIR } else { Join-Path $HOME ".local\bin" }

Write-Step "1" "Detecting platform & resolving release..."

$Arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
switch ($Arch) {
    "x64"   { $Target = "x86_64-pc-windows-msvc" }
    "arm64" { $Target = "aarch64-pc-windows-msvc" }
    default {
        if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") {
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

    if (Get-Command curl.exe -ErrorAction SilentlyContinue) {
        Write-Info "Downloading $Archive..."
        & curl.exe -fL --progress-bar "$BaseUrl/$Archive" -o $ZipPath
        if ($LASTEXITCODE -ne 0) {
            throw "Failed to download $BaseUrl/$Archive"
        }
        if ($env:TES_SKIP_CHECKSUM -ne "1") {
            & curl.exe -fsSL "$BaseUrl/$Archive.sha256" -o $ShaPath
            if ($LASTEXITCODE -ne 0) {
                throw "Failed to download $BaseUrl/$Archive.sha256"
            }
        }
        Write-Success "Downloaded $Archive"
    } else {
        Write-Info "Downloading $Archive..."
        $ProgressPreference = "Continue"
        Invoke-WebRequest -Uri "$BaseUrl/$Archive" -OutFile $ZipPath -UseBasicParsing
        if ($env:TES_SKIP_CHECKSUM -ne "1") {
            Invoke-WebRequest -Uri "$BaseUrl/$Archive.sha256" -OutFile $ShaPath -UseBasicParsing
        }
        Write-Success "Downloaded $Archive"
    }

    if ($env:TES_SKIP_CHECKSUM -ne "1") {
        if (-not (Test-Path $ShaPath)) {
            throw "Missing SHA-256 checksum file for $Archive"
        }
        $ExpectedHash = ((Get-Content $ShaPath -Raw).Trim() -split '\s+')[0].ToLowerInvariant()
        $ActualHash = (Get-FileHash -Path $ZipPath -Algorithm SHA256).Hash.ToLowerInvariant()
        if ((-not $ExpectedHash) -or ($ActualHash -ne $ExpectedHash)) {
            throw "SHA-256 checksum mismatch for $Archive (expected $ExpectedHash, got $ActualHash)"
        }
        Write-Success "Verified SHA-256 checksum"
    }

    Write-Step "3" "Installing binary..."

    Expand-Archive -Path $ZipPath -DestinationPath $TmpDir -Force
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

    $DestBin = Join-Path $InstallDir "tes.exe"
    Copy-Item -Path (Join-Path $TmpDir "tes.exe") -Destination $DestBin -Force

    if (-not $env:TES_INSTALL_DIR) {
        $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
        if (-not $UserPath) {
            $UserPath = ""
        }
        $PathEntries = $UserPath -split ";" | Where-Object { $_ -ne "" }
        if ($PathEntries -notcontains $InstallDir) {
            $NewUserPath = if ($UserPath) { "$UserPath;$InstallDir" } else { $InstallDir }
            [Environment]::SetEnvironmentVariable("Path", $NewUserPath, "User")
            Write-Success "Added $InstallDir to user PATH"
        }
    }

    if (($env:PATH -split ";") -notcontains $InstallDir) {
        $env:PATH = "$InstallDir;$env:PATH"
    }

    Write-Success "Installed tes to $DestBin"
    Write-Info "Run 'tes env --install' to configure PowerShell integration."
} finally {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
}
