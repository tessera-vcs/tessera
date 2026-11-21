//! Self-updater (`tes upgrade` / `tes update` / `tes --upgrade`) with SHA-256 integrity
//! verification, pure-Rust `.tar.gz` and `.zip` archive extraction, and atomic cross-platform
//! executable replacement.

use std::cmp::Ordering;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use anyhow::{Context, Result, anyhow, bail};
use flate2::Crc;
use flate2::read::{DeflateDecoder, GzDecoder};
use serde_json::{Value, json};

use crate::output::{CliContext, c_bold, c_cyan, c_dim, c_green, c_yellow};

/// Default GitHub repository slug hosting official Tessera releases.
const DEFAULT_GITHUB_REPO: &str = "tessera-vcs/tessera";

/// Maximum compressed or decompressed binary size accepted during self-upgrade (256 MiB).
const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;

/// Process-local sequence counter for unique temporary staging filenames.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Options controlling `tes upgrade` execution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpgradeOptions {
    /// Check for a newer release without downloading or modifying the binary.
    pub check: bool,
    /// Optional explicit version or tag to install (e.g., `0.0.3` or `v0.0.3`).
    pub version: Option<String>,
    /// Reinstall even when the running version is already equal to or newer than the target.
    pub force: bool,
}

/// Supported release archive container formats matching `.github/workflows/release.yml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    /// Gzipped POSIX tar archive (`tes-<target>.tar.gz`) used on macOS, Linux, and BSDs.
    TarGz,
    /// ZIP archive (`tes-<target>.zip`) used on Windows.
    Zip,
}

/// Platform release target metadata used to resolve download URLs and binary filenames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseTarget {
    /// Rust target triple embedded in GitHub Release asset filenames.
    pub triple: &'static str,
    /// Archive container format for this target.
    pub format: ArchiveFormat,
    /// Expected executable filename inside the release archive.
    pub binary_name: &'static str,
}

impl ReleaseTarget {
    /// Returns the release archive filename (e.g., `tes-aarch64-apple-darwin.tar.gz`).
    #[must_use]
    pub fn archive_name(&self) -> String {
        match self.format {
            ArchiveFormat::TarGz => format!("tes-{}.tar.gz", self.triple),
            ArchiveFormat::Zip => format!("tes-{}.zip", self.triple),
        }
    }
}

/// Parsed SemVer 2.0.0 version triple with optional pre-release suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemVer {
    /// Major version component.
    pub major: u64,
    /// Minor version component.
    pub minor: u64,
    /// Patch version component.
    pub patch: u64,
    /// Optional pre-release identifier (without leading `-`).
    pub pre: Option<String>,
}

impl SemVer {
    /// Parses a version string with an optional leading `v` (e.g., `v0.0.3` or `1.2.3-rc.1`).
    ///
    /// Rejects whitespace, path separators, or non-SemVer characters so user-supplied `--version`
    /// values can never inject URL path segments or query parameters.
    ///
    /// # Errors
    /// Returns an error if `raw` is not a valid SemVer 2.0.0 string.
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        let stripped = trimmed.strip_prefix('v').unwrap_or(trimmed);

        if stripped.is_empty()
            || !stripped
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
        {
            bail!("invalid version '{raw}': expected semantic version such as '0.0.3' or 'v0.0.3'");
        }

        let without_build = stripped.split('+').next().unwrap_or(stripped);
        let (core, pre) = match without_build.split_once('-') {
            Some((c, p)) => {
                if p.is_empty() || p.split('.').any(str::is_empty) {
                    bail!("invalid pre-release identifier in version '{raw}'");
                }

                (c, Some(p.to_string()))
            }
            None => (without_build, None),
        };

        let mut parts = core.split('.');
        let major_s = parts
            .next()
            .ok_or_else(|| anyhow!("missing major version in '{raw}'"))?;
        let minor_s = parts
            .next()
            .ok_or_else(|| anyhow!("missing minor version in '{raw}'"))?;
        let patch_s = parts
            .next()
            .ok_or_else(|| anyhow!("missing patch version in '{raw}'"))?;

        if parts.next().is_some() {
            bail!("invalid version '{raw}': expected MAJOR.MINOR.PATCH");
        }

        let major = major_s
            .parse::<u64>()
            .with_context(|| format!("invalid major version in '{raw}'"))?;
        let minor = minor_s
            .parse::<u64>()
            .with_context(|| format!("invalid minor version in '{raw}'"))?;
        let patch = patch_s
            .parse::<u64>()
            .with_context(|| format!("invalid patch version in '{raw}'"))?;

        Ok(Self {
            major,
            minor,
            patch,
            pre,
        })
    }

    /// Formats the canonical version string without a leading `v`.
    #[must_use]
    pub fn normalized(&self) -> String {
        match &self.pre {
            Some(pre) => format!("{}.{}.{}-{pre}", self.major, self.minor, self.patch),
            None => format!("{}.{}.{}", self.major, self.minor, self.patch),
        }
    }

    /// Formats the Git release tag (`vMAJOR.MINOR.PATCH[-pre]`).
    #[must_use]
    pub fn tag(&self) -> String {
        format!("v{}", self.normalized())
    }
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => compare_prerelease(a, b),
            })
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Compares two SemVer 2.0.0 pre-release identifier chains.
fn compare_prerelease(a: &str, b: &str) -> Ordering {
    let mut a_parts = a.split('.');
    let mut b_parts = b.split('.');

    loop {
        match (a_parts.next(), b_parts.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ap), Some(bp)) => {
                let ord = match (ap.parse::<u64>(), bp.parse::<u64>()) {
                    (Ok(an), Ok(bn)) => an.cmp(&bn),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => ap.cmp(bp),
                };

                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

/// Resolves the official release target metadata for `(os, arch)`.
///
/// Linux targets default to static `musl` archives matching `docs/public/install.sh` so upgraded
/// binaries work across all glibc and musl distributions without dynamic linker version issues.
///
/// # Errors
/// Returns an error if no prebuilt binary is published for `(os, arch)`.
pub fn detect_release_target_for(os: &str, arch: &str) -> Result<ReleaseTarget> {
    match (os, arch) {
        ("macos", "aarch64") => Ok(ReleaseTarget {
            triple: "aarch64-apple-darwin",
            format: ArchiveFormat::TarGz,
            binary_name: "tes",
        }),
        ("macos", "x86_64") => Ok(ReleaseTarget {
            triple: "x86_64-apple-darwin",
            format: ArchiveFormat::TarGz,
            binary_name: "tes",
        }),
        ("linux", "x86_64") => Ok(ReleaseTarget {
            triple: "x86_64-unknown-linux-musl",
            format: ArchiveFormat::TarGz,
            binary_name: "tes",
        }),
        ("linux", "aarch64") => Ok(ReleaseTarget {
            triple: "aarch64-unknown-linux-musl",
            format: ArchiveFormat::TarGz,
            binary_name: "tes",
        }),
        ("windows", "x86_64") => Ok(ReleaseTarget {
            triple: "x86_64-pc-windows-msvc",
            format: ArchiveFormat::Zip,
            binary_name: "tes.exe",
        }),
        ("windows", "aarch64") => Ok(ReleaseTarget {
            triple: "aarch64-pc-windows-msvc",
            format: ArchiveFormat::Zip,
            binary_name: "tes.exe",
        }),
        ("freebsd", "x86_64") => Ok(ReleaseTarget {
            triple: "x86_64-unknown-freebsd",
            format: ArchiveFormat::TarGz,
            binary_name: "tes",
        }),
        ("openbsd", "x86_64") => Ok(ReleaseTarget {
            triple: "x86_64-unknown-openbsd",
            format: ArchiveFormat::TarGz,
            binary_name: "tes",
        }),
        _ => bail!(
            "no prebuilt release binary is published for {os}/{arch}; build from source with Cargo"
        ),
    }
}

/// Validates a GitHub `owner/repo` slug against safe characters.
fn validate_github_repo(repo: &str) -> Result<String> {
    let trimmed = repo.trim();
    let Some((owner, name)) = trimmed.split_once('/') else {
        bail!("invalid TES_GITHUB_REPO '{repo}': expected 'owner/repo'");
    };

    let valid_part = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };

    if !valid_part(owner) || !valid_part(name) || name.contains('/') {
        bail!("invalid TES_GITHUB_REPO '{repo}': expected 'owner/repo'");
    }

    Ok(format!("{owner}/{name}"))
}

/// Validates that an override URL uses `http://` or `https://` with no control characters.
fn validate_http_url(raw: &str, env_name: &str) -> Result<String> {
    let trimmed = raw.trim();

    if !(trimmed.starts_with("https://") || trimmed.starts_with("http://"))
        || trimmed.chars().any(char::is_control)
    {
        bail!("invalid {env_name} '{raw}': must start with http:// or https://");
    }

    Ok(trimmed.trim_end_matches('/').to_string())
}

/// Removes leftover `.old` executable backups from prior self-upgrades on Windows.
pub fn cleanup_stale_windows_backup() {
    #[cfg(windows)]
    {
        if let Ok(exe) = std::env::current_exe() {
            let target = fs::canonicalize(&exe).unwrap_or(exe);

            if let (Some(parent), Some(file_name)) =
                (target.parent(), target.file_name().and_then(|n| n.to_str()))
            {
                let old_exact = parent.join(format!("{file_name}.old"));
                let _ = fs::remove_file(&old_exact);

                let prefix = format!("{file_name}.old.");

                if let Ok(entries) = fs::read_dir(parent) {
                    for entry in entries.flatten() {
                        if let Some(name) = entry.file_name().to_str() {
                            if name.starts_with(&prefix) {
                                let _ = fs::remove_file(entry.path());
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Executes `tes upgrade` (`tes update`, `tes --upgrade`, `tes --update`).
///
/// Resolves the target release version, compares it against `CARGO_PKG_VERSION`, downloads and
/// SHA-256-verifies the platform archive, extracts the executable in memory/temp storage, and
/// atomically replaces the currently running `tes` binary.
///
/// # Errors
/// Returns an error if version resolution, download, SHA-256 verification, extraction, or
/// filesystem replacement fails.
pub fn run_upgrade(ctx: &CliContext, opts: UpgradeOptions) -> Result<()> {
    let current = SemVer::parse(env!("CARGO_PKG_VERSION"))?;
    let target = detect_release_target_for(std::env::consts::OS, std::env::consts::ARCH)?;
    let repo = match std::env::var("TES_GITHUB_REPO") {
        Ok(val) if !val.trim().is_empty() => validate_github_repo(&val)?,
        _ => DEFAULT_GITHUB_REPO.to_string(),
    };

    let desired = match &opts.version {
        Some(raw) => SemVer::parse(raw)?,
        None => resolve_latest_release_version(&repo)?,
    };

    let current_exe = resolve_current_exe_target()?;
    let update_available = desired > current;
    let needs_install = if opts.force {
        true
    } else if opts.version.is_some() {
        desired != current
    } else {
        update_available
    };

    if opts.check {
        let status = if update_available {
            "update_available"
        } else if desired == current {
            "up_to_date"
        } else {
            "ahead"
        };

        if ctx.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "status": status,
                    "current_version": current.normalized(),
                    "target_version": desired.normalized(),
                    "target_tag": desired.tag(),
                    "update_available": update_available,
                    "target": target.triple,
                    "archive": target.archive_name(),
                    "binary_path": current_exe.display().to_string(),
                }))?
            );
        } else if update_available {
            println!(
                "{} Tessera update available: {} -> {} ({})",
                c_cyan("!"),
                c_dim(current.normalized()),
                c_green(desired.normalized()),
                target.triple
            );
            println!("  Run `{}` to install.", c_bold("tes upgrade"));
        } else if desired == current {
            println!(
                "{} Tessera {} is already up to date ({}).",
                c_green("✓"),
                c_bold(current.normalized()),
                target.triple
            );
        } else {
            println!(
                "{} Installed Tessera {} is newer than release {} ({}).",
                c_cyan("i"),
                c_bold(current.normalized()),
                desired.normalized(),
                target.triple
            );
        }

        return Ok(());
    }

    if !needs_install {
        let status = if desired == current {
            "up_to_date"
        } else {
            "ahead"
        };

        if ctx.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "status": status,
                    "upgraded": false,
                    "current_version": current.normalized(),
                    "target_version": desired.normalized(),
                    "target_tag": desired.tag(),
                    "target": target.triple,
                    "binary_path": current_exe.display().to_string(),
                }))?
            );
        } else if desired == current {
            println!(
                "{} Tessera {} is already up to date.",
                c_green("✓"),
                c_bold(current.normalized())
            );
        } else {
            println!(
                "{} Installed Tessera {} is already newer than {} (pass {} to downgrade/reinstall).",
                c_yellow("!"),
                c_bold(current.normalized()),
                desired.normalized(),
                c_bold("--force")
            );
        }

        return Ok(());
    }

    let archive_name = target.archive_name();
    let download_base = match std::env::var("TES_BASE_URL") {
        Ok(val) if !val.trim().is_empty() => validate_http_url(&val, "TES_BASE_URL")?,
        _ => format!(
            "https://github.com/{repo}/releases/download/{}",
            desired.tag()
        ),
    };
    let archive_url = format!("{download_base}/{archive_name}");
    let sha_url = format!("{download_base}/{archive_name}.sha256");

    if !ctx.json {
        println!(
            "{} Upgrading Tessera {} -> {} ({})...",
            c_cyan("->"),
            c_dim(current.normalized()),
            c_bold(desired.normalized()),
            target.triple
        );
    }

    let work_dir = create_upgrade_temp_dir()?;
    let work_guard = TempDirGuard(work_dir.clone());
    let archive_path = work_dir.join(&archive_name);
    let sha_path = work_dir.join(format!("{archive_name}.sha256"));

    download_to_file(&archive_url, &archive_path, false)
        .with_context(|| format!("failed to download release archive from {archive_url}"))?;
    download_to_file(&sha_url, &sha_path, false)
        .with_context(|| format!("failed to download SHA-256 checksum from {sha_url}"))?;

    let expected_sha = parse_sha256_file(&sha_path)?;
    let actual_sha = sha256_file(&archive_path)?;

    if actual_sha != expected_sha {
        bail!(
            "SHA-256 checksum mismatch for {archive_name}: expected {expected_sha}, got {actual_sha}"
        );
    }

    let binary_bytes = match target.format {
        ArchiveFormat::TarGz => extract_from_tar_gz(&archive_path, target.binary_name)?,
        ArchiveFormat::Zip => extract_from_zip(&archive_path, target.binary_name)?,
    };

    if binary_bytes.is_empty() {
        bail!(
            "extracted '{}' from {archive_name} is empty",
            target.binary_name
        );
    }

    replace_executable(&current_exe, &binary_bytes)?;
    drop(work_guard);

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "status": "upgraded",
                "upgraded": true,
                "previous_version": current.normalized(),
                "version": desired.normalized(),
                "tag": desired.tag(),
                "target": target.triple,
                "sha256": actual_sha,
                "binary_path": current_exe.display().to_string(),
            }))?
        );
    } else {
        println!(
            "{} Upgraded Tessera {} -> {} at {}",
            c_green("✓"),
            c_dim(current.normalized()),
            c_green(desired.normalized()),
            current_exe.display()
        );
    }

    Ok(())
}

/// Resolves the canonical target path for the currently running executable so symlinked launchers
/// update the underlying binary rather than replacing the symlink itself.
fn resolve_current_exe_target() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("failed to determine current executable path")?;
    Ok(fs::canonicalize(&exe).unwrap_or(exe))
}

/// Resolves the latest published GitHub Release version for `repo`, checking `TES_RELEASE_API_URL`
/// first, then GitHub's Releases API, and falling back to redirect-based resolution if rate-limited.
fn resolve_latest_release_version(repo: &str) -> Result<SemVer> {
    if let Ok(override_url) = std::env::var("TES_RELEASE_API_URL") {
        if !override_url.trim().is_empty() {
            let clean_url = validate_http_url(&override_url, "TES_RELEASE_API_URL")?;
            let raw = download_to_string(&clean_url, false)?;
            return parse_release_tag_from_json(&raw);
        }
    }

    let api_url = format!("https://api.github.com/repos/{repo}/releases/latest");

    if let Ok(raw) = download_to_string(&api_url, true) {
        if let Ok(ver) = parse_release_tag_from_json(&raw) {
            return Ok(ver);
        }
    }

    resolve_latest_tag_via_redirect(repo)
}

/// Extracts and parses `tag_name` from a GitHub Releases API JSON payload.
pub fn parse_release_tag_from_json(raw: &str) -> Result<SemVer> {
    let parsed: Value = serde_json::from_str(raw)
        .context("failed to parse release metadata JSON from releases endpoint")?;
    let tag = parsed
        .get("tag_name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("release metadata JSON is missing 'tag_name'"))?;

    SemVer::parse(tag)
}

/// Resolves the latest release tag by following `https://github.com/<repo>/releases/latest`
/// without consuming unauthenticated GitHub API rate-limit quota.
fn resolve_latest_tag_via_redirect(repo: &str) -> Result<SemVer> {
    let url = format!("https://github.com/{repo}/releases/latest");
    let curl_bin = if cfg!(windows) { "curl.exe" } else { "curl" };
    let null_dest = if cfg!(windows) { "NUL" } else { "/dev/null" };

    let out = Command::new(curl_bin)
        .args([
            "-fsSLI",
            "--connect-timeout",
            "15",
            "--max-time",
            "30",
            "-o",
            null_dest,
            "-w",
            "%{url_effective}",
            &url,
        ])
        .output()
        .with_context(|| format!("failed to query latest release from {url}"))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "failed to resolve latest release from {url}: {}",
            stderr.trim()
        );
    }

    let effective_url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let tag = effective_url
        .rsplit("/releases/tag/")
        .next()
        .filter(|s| *s != effective_url && !s.is_empty())
        .map(|s| s.split(['?', '#']).next().unwrap_or(s))
        .ok_or_else(|| {
            anyhow!("could not determine latest release tag from redirect URL '{effective_url}'")
        })?;

    SemVer::parse(tag)
}

/// Downloads `url` into memory as UTF-8 text.
fn download_to_string(url: &str, send_github_headers: bool) -> Result<String> {
    let work_dir = create_upgrade_temp_dir()?;
    let _guard = TempDirGuard(work_dir.clone());
    let out_path = work_dir.join("response.json");

    download_to_file(url, &out_path, send_github_headers)?;
    fs::read_to_string(&out_path).with_context(|| format!("failed to read response from {url}"))
}

/// Downloads `url` to `dest` using the platform's native HTTP client (`curl`, `wget`, BSD `ftp`,
/// or Windows `curl.exe` / PowerShell `Invoke-WebRequest`).
fn download_to_file(url: &str, dest: &Path, send_github_headers: bool) -> Result<()> {
    let user_agent = format!("tessera-cli/{}", env!("CARGO_PKG_VERSION"));
    let github_token = if send_github_headers && url.starts_with("https://api.github.com/") {
        std::env::var("GITHUB_TOKEN")
            .ok()
            .or_else(|| std::env::var("GH_TOKEN").ok())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty() && !t.chars().any(char::is_control))
    } else {
        None
    };

    #[cfg(windows)]
    {
        if let Ok(out) = run_curl_download(
            "curl.exe",
            url,
            dest,
            &user_agent,
            send_github_headers,
            github_token.as_deref(),
        ) {
            if out.status.success() && dest.is_file() {
                return Ok(());
            }

            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            bail!("curl.exe failed for {url}: {stderr}");
        }

        let ps_script = "$ProgressPreference = 'SilentlyContinue'; \
            $headers = @{ 'User-Agent' = $env:TES_DL_UA }; \
            if ($env:TES_DL_ACCEPT) { $headers['Accept'] = $env:TES_DL_ACCEPT }; \
            if ($env:TES_DL_AUTH) { $headers['Authorization'] = $env:TES_DL_AUTH }; \
            Invoke-WebRequest -Uri $env:TES_DL_URL -OutFile $env:TES_DL_DEST -Headers $headers -UseBasicParsing";

        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", ps_script])
            .env("TES_DL_URL", url)
            .env("TES_DL_DEST", dest)
            .env("TES_DL_UA", &user_agent);

        if send_github_headers {
            cmd.env("TES_DL_ACCEPT", "application/vnd.github+json");
        }

        if let Some(tok) = github_token.as_deref() {
            cmd.env("TES_DL_AUTH", format!("Bearer {tok}"));
        }

        let out = cmd
            .output()
            .context("neither curl.exe nor powershell is available to download release files")?;

        if out.status.success() && dest.is_file() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!("PowerShell download failed for {url}: {stderr}");
    }

    #[cfg(not(windows))]
    {
        if let Ok(out) = run_curl_download(
            "curl",
            url,
            dest,
            &user_agent,
            send_github_headers,
            github_token.as_deref(),
        ) {
            if out.status.success() && dest.is_file() {
                return Ok(());
            }

            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            bail!("curl failed for {url}: {stderr}");
        }

        let mut wget = Command::new("wget");
        wget.args(["-q", "-T", "15", "-U", &user_agent]);

        if send_github_headers {
            wget.args(["--header", "Accept: application/vnd.github+json"]);
        }

        if let Some(tok) = github_token.as_deref() {
            wget.args(["--header", &format!("Authorization: Bearer {tok}")]);
        }

        wget.arg("-O").arg(dest).arg(url);

        if let Ok(out) = wget.output() {
            if out.status.success() && dest.is_file() {
                return Ok(());
            }

            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            bail!("wget failed for {url}: {stderr}");
        }

        if cfg!(any(
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "macos"
        )) {
            if let Ok(out) = Command::new("ftp")
                .arg("-V")
                .arg("-o")
                .arg(dest)
                .arg(url)
                .output()
            {
                if out.status.success() && dest.is_file() {
                    return Ok(());
                }

                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                bail!("ftp failed for {url}: {stderr}");
            }
        }

        bail!("no HTTP client found (install curl or wget to run 'tes upgrade')");
    }
}

/// Runs `curl` / `curl.exe` with consistent timeouts, redirect handling, and headers.
fn run_curl_download(
    bin: &str,
    url: &str,
    dest: &Path,
    user_agent: &str,
    send_github_headers: bool,
    github_token: Option<&str>,
) -> io::Result<std::process::Output> {
    let mut cmd = Command::new(bin);
    cmd.args([
        "-fsSL",
        "--retry",
        "2",
        "--connect-timeout",
        "15",
        "--max-time",
        "300",
        "-A",
        user_agent,
    ]);

    if send_github_headers {
        cmd.args(["-H", "Accept: application/vnd.github+json"]);
    }

    if let Some(tok) = github_token {
        cmd.args(["-H", &format!("Authorization: Bearer {tok}")]);
    }

    cmd.arg("-o").arg(dest).arg(url).output()
}

/// Parses a `<hex>  <filename>` SHA-256 checksum file and validates it contains 64 hex chars.
pub fn parse_sha256_file(path: &Path) -> Result<String> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read checksum file '{}'", path.display()))?;
    let hash = raw
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();

    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!(
            "invalid SHA-256 checksum format in '{}' (expected 64 hex characters)",
            path.display()
        );
    }

    Ok(hash)
}

/// Computes the lowercase hexadecimal SHA-256 digest of a file on disk (FIPS 180-4).
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)
        .with_context(|| format!("failed to open '{}' for SHA-256 hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];

    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("failed to read '{}' for SHA-256 hashing", path.display()))?;

        if n == 0 {
            break;
        }

        hasher.update(&buf[..n]);
    }

    Ok(hasher.finalize_hex())
}

/// Computes the lowercase hexadecimal SHA-256 digest of `data` in memory (FIPS 180-4).
#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize_hex()
}

/// Pure-Rust FIPS 180-4 SHA-256 streaming hasher requiring zero external dependencies.
struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffer_len: usize,
    total_bytes: u64,
}

impl Sha256 {
    /// FIPS 180-4 initial hash values (first 32 bits of fractional parts of square roots of first 8 primes).
    const INIT: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];

    /// FIPS 180-4 round constants (first 32 bits of fractional parts of cube roots of first 64 primes).
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];

    /// Creates a fresh SHA-256 hasher state.
    fn new() -> Self {
        Self {
            state: Self::INIT,
            buffer: [0u8; 64],
            buffer_len: 0,
            total_bytes: 0,
        }
    }

    /// Feeds `chunk` bytes into the SHA-256 state.
    fn update(&mut self, mut chunk: &[u8]) {
        self.total_bytes = self.total_bytes.saturating_add(chunk.len() as u64);

        if self.buffer_len > 0 {
            let need = 64 - self.buffer_len;
            let take = need.min(chunk.len());
            self.buffer[self.buffer_len..self.buffer_len + take].copy_from_slice(&chunk[..take]);
            self.buffer_len += take;
            chunk = &chunk[take..];

            if self.buffer_len == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffer_len = 0;
            }
        }

        while chunk.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&chunk[..64]);
            self.compress(&block);
            chunk = &chunk[64..];
        }

        if !chunk.is_empty() {
            self.buffer[..chunk.len()].copy_from_slice(chunk);
            self.buffer_len = chunk.len();
        }
    }

    /// Compresses a single 512-bit block into the SHA-256 state.
    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];

        for (i, slot) in w.iter_mut().enumerate().take(16) {
            let base = i * 4;
            *slot = u32::from_be_bytes([
                block[base],
                block[base + 1],
                block[base + 2],
                block[base + 3],
            ]);
        }

        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;

        for (&k_i, &w_i) in Self::K.iter().zip(w.iter()) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(k_i)
                .wrapping_add(w_i);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        let deltas = [a, b, c, d, e, f, g, h];

        for (slot, delta) in self.state.iter_mut().zip(deltas) {
            *slot = slot.wrapping_add(delta);
        }
    }

    /// Finalizes padding and returns the 64-character lowercase hexadecimal digest.
    fn finalize_hex(mut self) -> String {
        use std::fmt::Write as _;

        let bit_len = self.total_bytes.wrapping_mul(8);
        self.buffer[self.buffer_len] = 0x80;
        self.buffer_len += 1;

        if self.buffer_len > 56 {
            self.buffer[self.buffer_len..64].fill(0);
            let block = self.buffer;
            self.compress(&block);
            self.buffer_len = 0;
        }

        self.buffer[self.buffer_len..56].fill(0);
        self.buffer[56..64].copy_from_slice(&bit_len.to_be_bytes());
        let final_block = self.buffer;
        self.compress(&final_block);

        let mut out = String::with_capacity(64);

        for word in self.state {
            let _ = write!(out, "{word:08x}");
        }

        out
    }
}

/// Extracts `binary_name` from a `.tar.gz` release archive in memory without touching the
/// filesystem, rejecting symlinks, path traversal, and oversized payloads.
pub fn extract_from_tar_gz(archive_path: &Path, binary_name: &str) -> Result<Vec<u8>> {
    let file = File::open(archive_path)
        .with_context(|| format!("failed to open archive '{}'", archive_path.display()))?;
    let mut decoder = GzDecoder::new(file);
    let mut header = [0u8; 512];

    loop {
        if !read_exact_or_eof(&mut decoder, &mut header)? {
            break;
        }

        if header.iter().all(|&b| b == 0) {
            break;
        }

        let name = parse_tar_path(&header)?;
        let size = parse_tar_octal(&header[124..136])?;
        let typeflag = header[156];
        let padded_remainder = ((512 - (size % 512)) % 512) as usize;

        let normalized = name.trim_start_matches("./");
        let is_regular = typeflag == b'0' || typeflag == 0;

        if normalized == binary_name {
            if !is_regular {
                bail!("refusing to extract non-regular archive entry '{name}'");
            }

            if size > MAX_BINARY_BYTES {
                bail!("archive entry '{name}' exceeds maximum permitted size");
            }

            let mut payload = vec![0u8; size as usize];
            decoder.read_exact(&mut payload).with_context(|| {
                format!(
                    "truncated archive payload for '{name}' in '{}'",
                    archive_path.display()
                )
            })?;

            return Ok(payload);
        }

        let skip_total = size.saturating_add(padded_remainder as u64);
        discard_exact(&mut decoder, skip_total)?;
    }

    bail!(
        "binary '{binary_name}' not found inside archive '{}'",
        archive_path.display()
    )
}

/// Reads an exact buffer length from `reader`, returning `Ok(false)` on clean EOF at byte 0.
fn read_exact_or_eof<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;

    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(false);
                }

                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unexpected EOF in tar archive",
                ));
            }
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }

    Ok(true)
}

/// Discards `count` bytes from `reader`.
fn discard_exact<R: Read>(reader: &mut R, mut count: u64) -> io::Result<()> {
    let mut scratch = [0u8; 4096];

    while count > 0 {
        let want = (count as usize).min(scratch.len());
        reader.read_exact(&mut scratch[..want])?;
        count -= want as u64;
    }

    Ok(())
}

/// Extracts and validates the path from a 512-byte POSIX ustar header.
fn parse_tar_path(header: &[u8; 512]) -> Result<String> {
    let name_bytes = header[0..100]
        .split(|&b| b == 0)
        .next()
        .unwrap_or(&header[0..100]);
    let name = std::str::from_utf8(name_bytes).context("non-UTF-8 path in tar header")?;

    let prefix_bytes = header[345..500]
        .split(|&b| b == 0)
        .next()
        .unwrap_or(&header[345..500]);
    let prefix = std::str::from_utf8(prefix_bytes).context("non-UTF-8 prefix in tar header")?;

    let full = if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    };

    if full.starts_with('/') || full.split('/').any(|seg| seg == "..") {
        bail!("unsafe archive member path '{full}'");
    }

    Ok(full)
}

/// Parses an ASCII octal field from a tar header (supporting optional NUL/space padding).
fn parse_tar_octal(field: &[u8]) -> Result<u64> {
    let trimmed = std::str::from_utf8(field)
        .context("non-UTF-8 octal size in tar header")?
        .trim_matches(|c: char| c == '\0' || c.is_ascii_whitespace());

    if trimmed.is_empty() {
        return Ok(0);
    }

    u64::from_str_radix(trimmed, 8)
        .with_context(|| format!("invalid octal size '{trimmed}' in tar header"))
}

/// Extracts `binary_name` from a `.zip` release archive in memory using the Central Directory,
/// verifying CRC-32 and rejecting path traversal, symlinks, and unsupported compression methods.
pub fn extract_from_zip(archive_path: &Path, binary_name: &str) -> Result<Vec<u8>> {
    let data = fs::read(archive_path)
        .with_context(|| format!("failed to read zip archive '{}'", archive_path.display()))?;

    if data.len() < 22 {
        bail!(
            "invalid zip archive '{}': too small",
            archive_path.display()
        );
    }

    let search_start = data.len().saturating_sub(22 + 65_535);
    let mut eocd_pos = None;

    for i in (search_start..=data.len() - 22).rev() {
        if &data[i..i + 4] == b"PK\x05\x06" {
            let comment_len = read_u16_le(&data, i + 20)? as usize;

            if i + 22 + comment_len == data.len() {
                eocd_pos = Some(i);
                break;
            }
        }
    }

    let eocd = eocd_pos.ok_or_else(|| {
        anyhow!(
            "invalid zip archive '{}': End-of-Central-Directory record not found",
            archive_path.display()
        )
    })?;

    let entry_count = read_u16_le(&data, eocd + 10)? as usize;
    let cd_size = read_u32_le(&data, eocd + 12)? as usize;
    let cd_offset = read_u32_le(&data, eocd + 16)? as usize;
    let cd_end = cd_offset
        .checked_add(cd_size)
        .ok_or_else(|| anyhow!("zip central directory overflow"))?;

    if cd_end > data.len() {
        bail!("invalid zip archive: central directory extends past EOF");
    }

    let mut pos = cd_offset;

    for _ in 0..entry_count {
        if pos + 46 > cd_end || &data[pos..pos + 4] != b"PK\x01\x02" {
            bail!("invalid zip central directory header");
        }

        let method = read_u16_le(&data, pos + 10)?;
        let expected_crc = read_u32_le(&data, pos + 16)?;
        let comp_size = read_u32_le(&data, pos + 20)? as usize;
        let uncomp_size = read_u32_le(&data, pos + 24)? as usize;
        let name_len = read_u16_le(&data, pos + 28)? as usize;
        let extra_len = read_u16_le(&data, pos + 30)? as usize;
        let comment_len = read_u16_le(&data, pos + 32)? as usize;
        let ext_attr = read_u32_le(&data, pos + 38)?;
        let local_offset = read_u32_le(&data, pos + 42)? as usize;

        let name_start = pos + 46;
        let name_end = name_start + name_len;
        let next_pos = name_end + extra_len + comment_len;

        if next_pos > cd_end {
            bail!("truncated zip central directory entry");
        }

        let raw_name =
            std::str::from_utf8(&data[name_start..name_end]).context("non-UTF-8 zip entry name")?;
        let normalized = raw_name.replace('\\', "/");

        if normalized.starts_with('/') || normalized.split('/').any(|seg| seg == "..") {
            bail!("unsafe zip entry path '{raw_name}'");
        }

        let unix_mode = (ext_attr >> 16) & 0o170_000;
        let is_symlink = unix_mode == 0o120_000;

        if normalized.trim_start_matches("./") == binary_name {
            if is_symlink {
                bail!("refusing to extract symlink entry '{raw_name}' from zip archive");
            }

            if (uncomp_size as u64) > MAX_BINARY_BYTES {
                bail!("zip entry '{raw_name}' exceeds maximum permitted size");
            }

            if local_offset + 30 > data.len()
                || &data[local_offset..local_offset + 4] != b"PK\x03\x04"
            {
                bail!("invalid zip local file header for '{raw_name}'");
            }

            let local_name_len = read_u16_le(&data, local_offset + 26)? as usize;
            let local_extra_len = read_u16_le(&data, local_offset + 28)? as usize;
            let data_start = local_offset
                .checked_add(30)
                .and_then(|v| v.checked_add(local_name_len))
                .and_then(|v| v.checked_add(local_extra_len))
                .ok_or_else(|| anyhow!("zip local header offset overflow"))?;
            let data_end = data_start
                .checked_add(comp_size)
                .ok_or_else(|| anyhow!("zip entry length overflow"))?;

            if data_end > data.len() {
                bail!("truncated zip payload for '{raw_name}'");
            }

            let compressed = &data[data_start..data_end];
            let payload = match method {
                0 => compressed.to_vec(),
                8 => {
                    let mut dec = DeflateDecoder::new(compressed);
                    let mut out = Vec::with_capacity(uncomp_size);
                    dec.read_to_end(&mut out)
                        .with_context(|| format!("failed to inflate zip entry '{raw_name}'"))?;
                    out
                }
                other => bail!("unsupported zip compression method {other} for '{raw_name}'"),
            };

            if payload.len() != uncomp_size {
                bail!(
                    "zip entry '{raw_name}' size mismatch: expected {uncomp_size}, got {}",
                    payload.len()
                );
            }

            let mut crc = Crc::new();
            crc.update(&payload);

            if crc.sum() != expected_crc {
                bail!(
                    "zip entry '{raw_name}' CRC-32 mismatch: expected {expected_crc:08x}, got {:08x}",
                    crc.sum()
                );
            }

            return Ok(payload);
        }

        pos = next_pos;
    }

    bail!(
        "binary '{binary_name}' not found inside zip archive '{}'",
        archive_path.display()
    )
}

/// Reads a little-endian `u16` from `buf` at `offset`.
fn read_u16_le(buf: &[u8], offset: usize) -> Result<u16> {
    let slice = buf
        .get(offset..offset + 2)
        .ok_or_else(|| anyhow!("unexpected EOF reading zip u16 at offset {offset}"))?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

/// Reads a little-endian `u32` from `buf` at `offset`.
fn read_u32_le(buf: &[u8], offset: usize) -> Result<u32> {
    let slice = buf
        .get(offset..offset + 4)
        .ok_or_else(|| anyhow!("unexpected EOF reading zip u32 at offset {offset}"))?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Atomically replaces the executable at `target_exe` with `new_bytes`.
///
/// On Unix, writes a sibling temp file with mode `0o755`, calls `sync_all()`, and atomically
/// renames over `target_exe` (avoiding Linux `ETXTBSY` and Apple Silicon code-signature crashes).
/// On Windows, renames the running `tes.exe` to `tes.exe.old`, moves the staged binary into place,
/// and rolls back the rename if placing the new binary fails.
pub fn replace_executable(target_exe: &Path, new_bytes: &[u8]) -> Result<()> {
    let parent = target_exe.parent().ok_or_else(|| {
        anyhow!(
            "cannot determine parent directory for '{}'",
            target_exe.display()
        )
    })?;
    let seq = TEMP_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    let pid = std::process::id();
    let stage_name = if cfg!(windows) {
        format!("tes.upgrade.{pid}.{seq}.tmp.exe")
    } else {
        format!(".tes.upgrade.{pid}.{seq}.tmp")
    };
    let stage_path = parent.join(stage_name);

    let write_res = (|| -> io::Result<()> {
        let mut file = File::create(&stage_path)?;
        file.write_all(new_bytes)?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = fs::remove_file(&stage_path);

        if err.kind() == io::ErrorKind::PermissionDenied {
            bail!(
                "permission denied writing to '{}': re-run with elevated permissions or reinstall into a user-writable directory",
                parent.display()
            );
        }

        return Err(err).with_context(|| {
            format!(
                "failed to stage upgraded binary at '{}'",
                stage_path.display()
            )
        });
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if let Err(err) = fs::set_permissions(&stage_path, fs::Permissions::from_mode(0o755)) {
            let _ = fs::remove_file(&stage_path);
            return Err(err).with_context(|| {
                format!(
                    "failed to set executable permissions on '{}'",
                    stage_path.display()
                )
            });
        }

        if let Err(err) = fs::rename(&stage_path, target_exe) {
            let _ = fs::remove_file(&stage_path);

            if err.kind() == io::ErrorKind::PermissionDenied {
                bail!(
                    "permission denied replacing '{}': re-run with elevated permissions or reinstall into a user-writable directory",
                    target_exe.display()
                );
            }

            return Err(err).with_context(|| {
                format!(
                    "failed to replace '{}' with upgraded binary",
                    target_exe.display()
                )
            });
        }
    }

    #[cfg(windows)]
    {
        let file_name = target_exe
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("tes.exe");
        let primary_backup = parent.join(format!("{file_name}.old"));
        let _ = fs::remove_file(&primary_backup);

        let backup_path = if primary_backup.exists() {
            parent.join(format!("{file_name}.old.{pid}.{seq}"))
        } else {
            primary_backup
        };

        if target_exe.exists() {
            if let Err(err) = fs::rename(target_exe, &backup_path) {
                let _ = fs::remove_file(&stage_path);
                return Err(err).with_context(|| {
                    format!(
                        "failed to move running executable '{}' aside to '{}'",
                        target_exe.display(),
                        backup_path.display()
                    )
                });
            }
        }

        if let Err(err) = fs::rename(&stage_path, target_exe) {
            let _ = fs::remove_file(&stage_path);

            if backup_path.exists() {
                let _ = fs::rename(&backup_path, target_exe);
            }

            return Err(err).with_context(|| {
                format!(
                    "failed to move upgraded binary into '{}'",
                    target_exe.display()
                )
            });
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        if let Err(err) = fs::rename(&stage_path, target_exe) {
            let _ = fs::remove_file(&stage_path);
            return Err(err).with_context(|| {
                format!(
                    "failed to replace '{}' with upgraded binary",
                    target_exe.display()
                )
            });
        }
    }

    Ok(())
}

/// Creates a unique temporary directory for download and verification artifacts.
fn create_upgrade_temp_dir() -> Result<PathBuf> {
    let seq = TEMP_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("tes-upgrade-{pid}-{seq}"));
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create temp directory '{}'", dir.display()))?;
    Ok(dir)
}

/// RAII cleanup guard removing temporary upgrade artifacts on drop.
struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::{DeflateEncoder, GzEncoder};

    fn build_tar_gz(entries: &[(&str, u8, &[u8])]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();

        for &(name, typeflag, content) in entries {
            let mut header = [0u8; 512];
            header[..name.len()].copy_from_slice(name.as_bytes());
            header[100..107].copy_from_slice(b"0000755");
            let size_oct = format!("{:011o}", content.len());
            header[124..135].copy_from_slice(size_oct.as_bytes());
            header[156] = typeflag;
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            header[148..156].fill(b' ');
            let csum: u32 = header.iter().map(|&b| u32::from(b)).sum();
            let csum_oct = format!("{csum:06o}\0 ");
            header[148..156].copy_from_slice(csum_oct.as_bytes());

            tar_bytes.extend_from_slice(&header);
            tar_bytes.extend_from_slice(content);

            let pad = (512 - (content.len() % 512)) % 512;
            tar_bytes.resize(tar_bytes.len() + pad, 0);
        }

        tar_bytes.resize(tar_bytes.len() + 1024, 0);

        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&tar_bytes).unwrap();
        gz.finish().unwrap()
    }

    fn build_zip(name: &str, content: &[u8], deflate: bool, unix_mode: u32) -> Vec<u8> {
        let mut crc = Crc::new();
        crc.update(content);
        let crc_val = crc.sum();

        let (method, compressed) = if deflate {
            let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
            enc.write_all(content).unwrap();
            (8u16, enc.finish().unwrap())
        } else {
            (0u16, content.to_vec())
        };

        let mut out = Vec::new();
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc_val.to_le_bytes());
        out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        out.extend_from_slice(&(content.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&compressed);

        let cd_offset = out.len() as u32;
        out.extend_from_slice(b"PK\x01\x02");
        out.extend_from_slice(&0x031eu16.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc_val.to_le_bytes());
        out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        out.extend_from_slice(&(content.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(unix_mode << 16).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        let cd_size = (out.len() as u32) - cd_offset;

        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());

        out
    }

    #[test]
    fn sha256_matches_nist_fips_180_4_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn semver_parsing_ordering_and_injection_rejection() {
        let v002 = SemVer::parse("0.0.2").unwrap();
        let v003_rc1 = SemVer::parse("v0.0.3-rc.1").unwrap();
        let v003_rc2 = SemVer::parse("v0.0.3-rc.2").unwrap();
        let v003_rc10 = SemVer::parse("v0.0.3-rc.10").unwrap();
        let v003 = SemVer::parse("v0.0.3").unwrap();
        let v010 = SemVer::parse("0.1.0").unwrap();

        assert!(v002 < v003_rc1);
        assert!(v003_rc1 < v003_rc2);
        assert!(v003_rc2 < v003_rc10);
        assert!(v003_rc10 < v003);
        assert!(v003 < v010);
        assert_eq!(v003.tag(), "v0.0.3");
        assert_eq!(v003.normalized(), "0.0.3");

        for bad in [
            "",
            "latest",
            "0.0.3/../../etc/passwd",
            "0.0.3?foo=bar",
            "1.2",
            "1.2.3.4",
            "1.2.3-",
        ] {
            assert!(
                SemVer::parse(bad).is_err(),
                "expected '{bad}' to be rejected"
            );
        }
    }

    #[test]
    fn release_target_detection_covers_all_published_platforms() {
        assert_eq!(
            detect_release_target_for("macos", "aarch64")
                .unwrap()
                .archive_name(),
            "tes-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            detect_release_target_for("macos", "x86_64")
                .unwrap()
                .archive_name(),
            "tes-x86_64-apple-darwin.tar.gz"
        );
        assert_eq!(
            detect_release_target_for("linux", "x86_64")
                .unwrap()
                .archive_name(),
            "tes-x86_64-unknown-linux-musl.tar.gz"
        );
        assert_eq!(
            detect_release_target_for("linux", "aarch64")
                .unwrap()
                .archive_name(),
            "tes-aarch64-unknown-linux-musl.tar.gz"
        );
        assert_eq!(
            detect_release_target_for("windows", "x86_64")
                .unwrap()
                .archive_name(),
            "tes-x86_64-pc-windows-msvc.zip"
        );
        assert_eq!(
            detect_release_target_for("windows", "aarch64")
                .unwrap()
                .archive_name(),
            "tes-aarch64-pc-windows-msvc.zip"
        );
        assert_eq!(
            detect_release_target_for("freebsd", "x86_64")
                .unwrap()
                .archive_name(),
            "tes-x86_64-unknown-freebsd.tar.gz"
        );
        assert_eq!(
            detect_release_target_for("openbsd", "x86_64")
                .unwrap()
                .archive_name(),
            "tes-x86_64-unknown-openbsd.tar.gz"
        );
        assert!(detect_release_target_for("freebsd", "aarch64").is_err());
        assert!(detect_release_target_for("solaris", "sparc64").is_err());
    }

    #[test]
    fn tar_gz_and_zip_extractors_validate_payloads_and_reject_traversal_or_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let payload = b"#!/bin/sh\necho tes-9.9.9\n";

        let tgz_path = tmp.path().join("ok.tar.gz");
        fs::write(&tgz_path, build_tar_gz(&[("./tes", b'0', payload)])).unwrap();
        assert_eq!(extract_from_tar_gz(&tgz_path, "tes").unwrap(), payload);

        let evil_tgz = tmp.path().join("evil.tar.gz");
        fs::write(&evil_tgz, build_tar_gz(&[("../tes", b'0', payload)])).unwrap();
        assert!(extract_from_tar_gz(&evil_tgz, "tes").is_err());

        let symlink_tgz = tmp.path().join("sym.tar.gz");
        fs::write(&symlink_tgz, build_tar_gz(&[("tes", b'2', b"/bin/sh")])).unwrap();
        assert!(extract_from_tar_gz(&symlink_tgz, "tes").is_err());

        let zip_deflated = tmp.path().join("deflate.zip");
        fs::write(
            &zip_deflated,
            build_zip("tes.exe", payload, true, 0o100_755),
        )
        .unwrap();
        assert_eq!(extract_from_zip(&zip_deflated, "tes.exe").unwrap(), payload);

        let zip_stored = tmp.path().join("stored.zip");
        fs::write(&zip_stored, build_zip("tes.exe", payload, false, 0o100_755)).unwrap();
        assert_eq!(extract_from_zip(&zip_stored, "tes.exe").unwrap(), payload);

        let evil_zip = tmp.path().join("evil.zip");
        fs::write(&evil_zip, build_zip("../tes.exe", payload, true, 0o100_755)).unwrap();
        assert!(extract_from_zip(&evil_zip, "tes.exe").is_err());

        let symlink_zip = tmp.path().join("sym.zip");
        fs::write(
            &symlink_zip,
            build_zip("tes.exe", b"target", false, 0o120_777),
        )
        .unwrap();
        assert!(extract_from_zip(&symlink_zip, "tes.exe").is_err());
    }
}
