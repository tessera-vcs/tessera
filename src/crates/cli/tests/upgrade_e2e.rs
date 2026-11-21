//! End-to-end tests for `tes upgrade` (`tes update`, `tes --upgrade`, `tes --update`)
//! exercising a real local HTTP server, real SHA-256 verification, real archive unpacking,
//! and live in-place replacement of a running `tes` executable copy.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use flate2::write::{DeflateEncoder, GzEncoder};
use flate2::{Compression, Crc};
use serde_json::Value;

/// Returns `(target_triple, archive_name, is_zip)` for the current test host platform.
fn host_release_asset() -> (&'static str, String, bool) {
    let (triple, is_zip) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => ("aarch64-apple-darwin", false),
        ("macos", "x86_64") => ("x86_64-apple-darwin", false),
        ("linux", "x86_64") => ("x86_64-unknown-linux-musl", false),
        ("linux", "aarch64") => ("aarch64-unknown-linux-musl", false),
        ("windows", "x86_64") => ("x86_64-pc-windows-msvc", true),
        ("windows", "aarch64") => ("aarch64-pc-windows-msvc", true),
        ("freebsd", "x86_64") => ("x86_64-unknown-freebsd", false),
        ("openbsd", "x86_64") => ("x86_64-unknown-openbsd", false),
        (os, arch) => panic!("unsupported test host {os}/{arch}"),
    };

    let archive = if is_zip {
        format!("tes-{triple}.zip")
    } else {
        format!("tes-{triple}.tar.gz")
    };

    (triple, archive, is_zip)
}

/// Computes lowercase hex SHA-256 independently using platform-agnostic FIPS 180-4 test logic.
fn sha256_hex(data: &[u8]) -> String {
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

    let mut padded = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    padded.push(0x80);

    while (padded.len() % 64) != 56 {
        padded.push(0);
    }

    padded.extend_from_slice(&bit_len.to_be_bytes());
    let mut state = INIT;
    let mut offset = 0;

    while offset + 64 <= padded.len() {
        let block = &padded[offset..offset + 64];
        let mut w = [0u32; 64];

        for (i, slot) in w.iter_mut().enumerate().take(16) {
            *slot = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
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

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;

        for (&k_i, &w_i) in K.iter().zip(w.iter()) {
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

        for (slot, delta) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(delta);
        }

        offset += 64;
    }

    state.iter().map(|w| format!("{w:08x}")).collect()
}

/// Builds a release archive (`.tar.gz` on Unix, `.zip` on Windows) containing `bin_bytes`.
fn build_host_archive(bin_bytes: &[u8], is_zip: bool) -> Vec<u8> {
    if is_zip {
        let name = "tes.exe";
        let mut crc = Crc::new();
        crc.update(bin_bytes);
        let crc_val = crc.sum();

        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
        enc.write_all(bin_bytes).expect("deflate zip");
        let compressed = enc.finish().expect("finish deflate");

        let mut out = Vec::new();
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&8u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc_val.to_le_bytes());
        out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        out.extend_from_slice(&(bin_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&compressed);

        let cd_offset = out.len() as u32;
        out.extend_from_slice(b"PK\x01\x02");
        out.extend_from_slice(&0x031eu16.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&8u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc_val.to_le_bytes());
        out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        out.extend_from_slice(&(bin_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(0o100_755u32 << 16).to_le_bytes());
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
    } else {
        let name = "tes";
        let mut tar_bytes = Vec::new();
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..107].copy_from_slice(b"0000755");
        let size_oct = format!("{:011o}", bin_bytes.len());
        header[124..135].copy_from_slice(size_oct.as_bytes());
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[148..156].fill(b' ');
        let csum: u32 = header.iter().map(|&b| u32::from(b)).sum();
        let csum_oct = format!("{csum:06o}\0 ");
        header[148..156].copy_from_slice(csum_oct.as_bytes());

        tar_bytes.extend_from_slice(&header);
        tar_bytes.extend_from_slice(bin_bytes);

        let pad = (512 - (bin_bytes.len() % 512)) % 512;
        tar_bytes.resize(tar_bytes.len() + pad + 1024, 0);

        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&tar_bytes).expect("gzip tar");
        gz.finish().expect("finish gzip")
    }
}

/// Constructs an executable replacement payload for the host OS.
///
/// On Unix, uses a POSIX shell script printing `tes 9.9.9`. On Windows, appends an inert PE
/// overlay marker to a copy of the compiled `tes.exe` binary so it stays directly runnable.
fn build_replacement_executable(src_tes: &Path) -> Vec<u8> {
    if cfg!(windows) {
        let mut bytes = fs::read(src_tes).expect("read compiled tes.exe");
        bytes.extend_from_slice(b"\nTESSERA_UPGRADE_E2E_MARKER_9_9_9\n");
        bytes
    } else {
        b"#!/bin/sh\necho \"tes 9.9.9\"\n".to_vec()
    }
}

/// Minimal localhost HTTP server serving static route payloads for release E2E tests.
struct MockReleaseServer {
    base_url: String,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockReleaseServer {
    fn start(routes: BTreeMap<String, Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock http server");
        let addr = listener.local_addr().expect("local addr");
        listener.set_nonblocking(true).expect("set nonblocking");

        let shutdown = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&shutdown);

        let handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let mut buf = [0u8; 4096];
                        let n = stream.read(&mut buf).unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let path = req
                            .lines()
                            .next()
                            .and_then(|l| l.split_whitespace().nth(1))
                            .unwrap_or("/");

                        if let Some(body) = routes.get(path) {
                            let header = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(header.as_bytes());
                            let _ = stream.write_all(body);
                        } else {
                            let resp = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found";
                            let _ = stream.write_all(resp);
                        }

                        let _ = stream.flush();
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            base_url: format!("http://{addr}"),
            shutdown,
            handle: Some(handle),
        }
    }
}

impl Drop for MockReleaseServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);

        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Copies the compiled `tes` binary into `dest_dir` so self-upgrade tests never mutate Cargo's binary.
fn copy_tes_to(dest_dir: &Path) -> std::path::PathBuf {
    let src = Path::new(env!("CARGO_BIN_EXE_tes"));
    let name = if cfg!(windows) { "tes.exe" } else { "tes" };
    let dst = dest_dir.join(name);
    fs::copy(src, &dst).expect("copy tes binary to tempdir");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dst, fs::Permissions::from_mode(0o755)).expect("chmod copied tes");
    }

    dst
}

#[test]
fn upgrade_check_self_replace_sha256_mismatch_and_up_to_date_e2e() {
    let (triple, archive_name, is_zip) = host_release_asset();
    let src_tes = Path::new(env!("CARGO_BIN_EXE_tes"));
    let replacement = build_replacement_executable(src_tes);
    let archive_bytes = build_host_archive(&replacement, is_zip);
    let valid_sha = sha256_hex(&archive_bytes);
    let current_ver = env!("CARGO_PKG_VERSION");

    let mut routes = BTreeMap::new();
    routes.insert(
        "/api/latest-newer".to_string(),
        br#"{"tag_name":"v9.9.9"}"#.to_vec(),
    );
    routes.insert(
        "/api/latest-current".to_string(),
        format!(r#"{{"tag_name":"v{current_ver}"}}"#).into_bytes(),
    );
    routes.insert(
        "/api/latest-older".to_string(),
        br#"{"tag_name":"v0.0.1"}"#.to_vec(),
    );
    routes.insert(format!("/dl/{archive_name}"), archive_bytes.clone());
    routes.insert(
        format!("/dl/{archive_name}.sha256"),
        format!("{valid_sha}  {archive_name}\n").into_bytes(),
    );
    routes.insert(format!("/bad/{archive_name}"), archive_bytes);
    routes.insert(
        format!("/bad/{archive_name}.sha256"),
        format!("{}  {archive_name}\n", "0".repeat(64)).into_bytes(),
    );

    let server = MockReleaseServer::start(routes);

    // `--check --json` reports update available without touching the binary.
    let tmp = tempfile::tempdir().expect("tempdir");
    let copied_bin = copy_tes_to(tmp.path());
    let orig_bytes = fs::read(&copied_bin).expect("read original binary");

    let check_out = Command::new(&copied_bin)
        .args(["--json", "--update", "--check"])
        .env(
            "TES_RELEASE_API_URL",
            format!("{}/api/latest-newer", server.base_url),
        )
        .output()
        .expect("run tes --update --check");
    assert!(
        check_out.status.success(),
        "check failed: stderr={}",
        String::from_utf8_lossy(&check_out.stderr)
    );
    let check_json: Value = serde_json::from_slice(&check_out.stdout).expect("parse check json");
    assert_eq!(check_json["status"], "update_available");
    assert_eq!(check_json["update_available"], true);
    assert_eq!(check_json["current_version"], current_ver);
    assert_eq!(check_json["target_version"], "9.9.9");
    assert_eq!(check_json["target"], triple);
    assert_eq!(
        fs::read(&copied_bin).expect("read after check"),
        orig_bytes,
        "--check must never modify the binary"
    );

    // Already up-to-date and ahead-of-release are clean no-ops without `--force`.
    for (endpoint, expected_status) in [
        ("/api/latest-current", "up_to_date"),
        ("/api/latest-older", "ahead"),
    ] {
        let out = Command::new(&copied_bin)
            .args(["--json", "upgrade"])
            .env(
                "TES_RELEASE_API_URL",
                format!("{}{endpoint}", server.base_url),
            )
            .env("TES_BASE_URL", format!("{}/nonexistent", server.base_url))
            .output()
            .expect("run no-op upgrade");
        assert!(
            out.status.success(),
            "expected no-op success for {endpoint}: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let payload: Value = serde_json::from_slice(&out.stdout).expect("parse no-op json");
        assert_eq!(payload["status"], expected_status);
        assert_eq!(payload["upgraded"], false);
        assert_eq!(fs::read(&copied_bin).expect("read after no-op"), orig_bytes);
    }

    // SHA-256 mismatch aborts and leaves the original binary intact.
    let bad_out = Command::new(&copied_bin)
        .args(["upgrade"])
        .env(
            "TES_RELEASE_API_URL",
            format!("{}/api/latest-newer", server.base_url),
        )
        .env("TES_BASE_URL", format!("{}/bad", server.base_url))
        .output()
        .expect("run tampered upgrade");
    assert!(
        !bad_out.status.success(),
        "tampered SHA-256 must fail upgrade"
    );
    let bad_err = String::from_utf8_lossy(&bad_out.stderr);
    assert!(
        bad_err.contains("SHA-256 checksum mismatch"),
        "expected SHA-256 mismatch error, got: {bad_err}"
    );
    assert_eq!(
        fs::read(&copied_bin).expect("read after mismatch"),
        orig_bytes,
        "failed SHA-256 check must never overwrite the binary"
    );

    // Full live self-upgrade replaces the running binary atomically and passes `--version`.
    let up_out = Command::new(&copied_bin)
        .args(["--json", "--upgrade"])
        .env(
            "TES_RELEASE_API_URL",
            format!("{}/api/latest-newer", server.base_url),
        )
        .env("TES_BASE_URL", format!("{}/dl", server.base_url))
        .output()
        .expect("run live self-upgrade");
    assert!(
        up_out.status.success(),
        "self-upgrade failed: stdout={} stderr={}",
        String::from_utf8_lossy(&up_out.stdout),
        String::from_utf8_lossy(&up_out.stderr)
    );
    let up_json: Value = serde_json::from_slice(&up_out.stdout).expect("parse upgrade json");
    assert_eq!(up_json["status"], "upgraded");
    assert_eq!(up_json["upgraded"], true);
    assert_eq!(up_json["version"], "9.9.9");
    assert_eq!(up_json["sha256"], valid_sha);
    assert_eq!(
        fs::read(&copied_bin).expect("read upgraded binary"),
        replacement,
        "upgraded binary bytes must match extracted release payload"
    );

    let ver_out = Command::new(&copied_bin)
        .arg("--version")
        .output()
        .expect("execute upgraded binary");
    assert!(
        ver_out.status.success(),
        "upgraded binary failed to run: {}",
        String::from_utf8_lossy(&ver_out.stderr)
    );

    if !cfg!(windows) {
        assert_eq!(String::from_utf8_lossy(&ver_out.stdout).trim(), "tes 9.9.9");
    } else {
        // Running the upgraded Windows binary must also clean up `tes.exe.old`.
        assert!(
            !tmp.path().join("tes.exe.old").exists(),
            "stale tes.exe.old backup should be cleaned up on next invocation"
        );
    }

    // No `.tmp` staging files should remain beside the binary.
    for entry in fs::read_dir(tmp.path()).expect("read tempdir").flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        assert!(
            !name.contains(".upgrade."),
            "leftover temporary staging file found: {name}"
        );
    }

    // Pinned `--version` with `--force` installs without querying `/releases/latest`.
    let tmp_pinned = tempfile::tempdir().expect("tempdir pinned");
    let copied_pinned = copy_tes_to(tmp_pinned.path());
    let pin_out = Command::new(&copied_pinned)
        .args(["update", "--version", current_ver, "--force"])
        .env(
            "TES_RELEASE_API_URL",
            format!("{}/should-not-be-called", server.base_url),
        )
        .env("TES_BASE_URL", format!("{}/dl", server.base_url))
        .output()
        .expect("run forced pinned update");
    assert!(
        pin_out.status.success(),
        "forced pinned update failed: {}",
        String::from_utf8_lossy(&pin_out.stderr)
    );
    assert_eq!(
        fs::read(&copied_pinned).expect("read pinned upgraded binary"),
        replacement
    );

    // On Unix, upgrading through a symlink replaces the canonical target and keeps the symlink.
    #[cfg(unix)]
    {
        let tmp_sym = tempfile::tempdir().expect("tempdir symlink");
        let real_dir = tmp_sym.path().join("real");
        let bin_dir = tmp_sym.path().join("bin");
        fs::create_dir_all(&real_dir).expect("mkdir real");
        fs::create_dir_all(&bin_dir).expect("mkdir bin");
        let real_bin = copy_tes_to(&real_dir);
        let link_bin = bin_dir.join("tes");
        std::os::unix::fs::symlink(&real_bin, &link_bin).expect("create symlink");

        let sym_out = Command::new(&link_bin)
            .args(["upgrade"])
            .env(
                "TES_RELEASE_API_URL",
                format!("{}/api/latest-newer", server.base_url),
            )
            .env("TES_BASE_URL", format!("{}/dl", server.base_url))
            .output()
            .expect("upgrade through symlink");
        assert!(
            sym_out.status.success(),
            "symlink upgrade failed: {}",
            String::from_utf8_lossy(&sym_out.stderr)
        );
        assert!(
            fs::symlink_metadata(&link_bin)
                .expect("lstat symlink")
                .file_type()
                .is_symlink(),
            "launcher symlink must remain a symlink after upgrade"
        );
        assert_eq!(
            fs::read(&real_bin).expect("read canonical target"),
            replacement
        );
    }
}
