# Cross-platform PowerShell installer for Tessera (`tes.exe`) on Windows (x86_64 and ARM64).
$ErrorActionPreference = "Stop"

$Repo = if ($env:TES_GITHUB_REPO) { $env:TES_GITHUB_REPO } else { "tessera-vcs/tessera" }
$Version = if ($env:TES_VERSION) { $env:TES_VERSION } else { "latest" }
$InstallDir = if ($env:TES_INSTALL_DIR) { $env:TES_INSTALL_DIR } else { Join-Path $HOME ".local\bin" }

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
if ($Version -eq "latest") {
    $BaseUrl = "https://github.com/$Repo/releases/latest/download"
} else {
    $BaseUrl = "https://github.com/$Repo/releases/download/$Version"
}

$TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ("tes-install-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $TmpDir | Out-Null

try {
    $ZipPath = Join-Path $TmpDir $Archive
    $ShaPath = Join-Path $TmpDir "$Archive.sha256"

    Write-Host "==> Downloading $Archive from $BaseUrl..."
    Invoke-WebRequest -Uri "$BaseUrl/$Archive" -OutFile $ZipPath -UseBasicParsing
    if ($env:TES_SKIP_CHECKSUM -ne "1") {
        Invoke-WebRequest -Uri "$BaseUrl/$Archive.sha256" -OutFile $ShaPath -UseBasicParsing
        if (-not (Test-Path $ShaPath)) {
            throw "Missing SHA-256 checksum file for $Archive"
        }
        $ExpectedHash = ((Get-Content $ShaPath -Raw).Trim() -split '\s+')[0].ToLowerInvariant()
        $ActualHash = (Get-FileHash -Path $ZipPath -Algorithm SHA256).Hash.ToLowerInvariant()
        if ((-not $ExpectedHash) -or ($ActualHash -ne $ExpectedHash)) {
            throw "SHA-256 checksum mismatch for $Archive (expected $ExpectedHash, got $ActualHash)"
        }
    }

    Expand-Archive -Path $ZipPath -DestinationPath $TmpDir -Force
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

    $DestBin = Join-Path $InstallDir "tes.exe"
    Copy-Item -Path (Join-Path $TmpDir "tes.exe") -Destination $DestBin -Force

    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (-not $UserPath) {
        $UserPath = ""
    }
    $PathEntries = $UserPath -split ";" | Where-Object { $_ -ne "" }
    if ($PathEntries -notcontains $InstallDir) {
        $NewUserPath = if ($UserPath) { "$UserPath;$InstallDir" } else { $InstallDir }
        [Environment]::SetEnvironmentVariable("Path", $NewUserPath, "User")
    }
    if (($env:PATH -split ";") -notcontains $InstallDir) {
        $env:PATH = "$InstallDir;$env:PATH"
    }

    Write-Host "✓ Installed tes to $DestBin"
    try {
        & $DestBin env --install
    } catch {
        # Ignore profile write warnings
    }
    Write-Host "✓ Configured PowerShell prompt integration (restart your terminal or reload `$PROFILE)."
} finally {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
}
