#!/bin/sh
# Cross-platform installer for Tessera (`tes`) across macOS, Linux, FreeBSD, OpenBSD, and NetBSD.
set -eu

REPO="${TES_GITHUB_REPO:-getLunaris/tessera}"
VERSION="${TES_VERSION:-latest}"
INSTALL_DIR="${TES_INSTALL_DIR:-$HOME/.local/bin}"

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$ARCH" in
  x86_64|amd64)
    RUST_ARCH="x86_64"
    ;;
  arm64|aarch64)
    RUST_ARCH="aarch64"
    ;;
  *)
    echo "Error: unsupported architecture '$ARCH'" >&2
    exit 1
    ;;
esac

case "$OS" in
  Darwin)
    TARGET="${RUST_ARCH}-apple-darwin"
    ;;
  Linux)
    # Use static musl binary for maximum portability across glibc, Alpine/musl, and NixOS.
    TARGET="${RUST_ARCH}-unknown-linux-musl"
    ;;
  FreeBSD)
    TARGET="${RUST_ARCH}-unknown-freebsd"
    ;;
  OpenBSD)
    TARGET="${RUST_ARCH}-unknown-openbsd"
    ;;
  NetBSD)
    TARGET="${RUST_ARCH}-unknown-netbsd"
    ;;
  *)
    echo "Error: unsupported operating system '$OS'" >&2
    exit 1
    ;;
esac

ARCHIVE="tes-${TARGET}.tar.gz"
if [ "$VERSION" = "latest" ]; then
  BASE_URL="https://github.com/${REPO}/releases/latest/download"
else
  BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"
fi

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/tes-install.XXXXXX")"
trap 'rm -rf "$TMP_DIR"' EXIT

echo "==> Downloading ${ARCHIVE} from ${BASE_URL}..."
if command -v curl >/dev/null 2>&1; then
  curl -fsSL "${BASE_URL}/${ARCHIVE}" -o "${TMP_DIR}/${ARCHIVE}"
  if [ "${TES_SKIP_CHECKSUM:-0}" != "1" ]; then
    curl -fsSL "${BASE_URL}/${ARCHIVE}.sha256" -o "${TMP_DIR}/${ARCHIVE}.sha256"
  fi
elif command -v wget >/dev/null 2>&1; then
  wget -qO "${TMP_DIR}/${ARCHIVE}" "${BASE_URL}/${ARCHIVE}"
  if [ "${TES_SKIP_CHECKSUM:-0}" != "1" ]; then
    wget -qO "${TMP_DIR}/${ARCHIVE}.sha256" "${BASE_URL}/${ARCHIVE}.sha256"
  fi
else
  echo "Error: curl or wget is required to download Tessera." >&2
  exit 1
fi

if [ "${TES_SKIP_CHECKSUM:-0}" != "1" ]; then
  if [ ! -s "${TMP_DIR}/${ARCHIVE}.sha256" ]; then
    echo "Error: missing or empty SHA-256 checksum file for ${ARCHIVE}" >&2
    exit 1
  fi
  EXPECTED_HASH="$(awk '{print $1}' "${TMP_DIR}/${ARCHIVE}.sha256")"
  ACTUAL_HASH=""
  if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL_HASH="$(sha256sum "${TMP_DIR}/${ARCHIVE}" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    ACTUAL_HASH="$(shasum -a 256 "${TMP_DIR}/${ARCHIVE}" | awk '{print $1}')"
  elif command -v sha256 >/dev/null 2>&1; then
    ACTUAL_HASH="$(sha256 -q "${TMP_DIR}/${ARCHIVE}" 2>/dev/null || sha256 -r "${TMP_DIR}/${ARCHIVE}" | awk '{print $1}')"
  elif command -v cksum >/dev/null 2>&1; then
    ACTUAL_HASH="$(cksum -a sha256 -n "${TMP_DIR}/${ARCHIVE}" 2>/dev/null | awk '{print $1}')"
  fi

  if [ -z "$ACTUAL_HASH" ]; then
    echo "Error: no SHA-256 verification tool (sha256sum, shasum, sha256, or cksum) found." >&2
    exit 1
  fi

  if [ "$ACTUAL_HASH" != "$EXPECTED_HASH" ]; then
    echo "Error: SHA-256 checksum mismatch for ${ARCHIVE}" >&2
    echo "  Expected: ${EXPECTED_HASH}" >&2
    echo "  Actual:   ${ACTUAL_HASH}" >&2
    exit 1
  fi
fi

tar -xzf "${TMP_DIR}/${ARCHIVE}" -C "${TMP_DIR}"
mkdir -p "${INSTALL_DIR}"
cp "${TMP_DIR}/tes" "${INSTALL_DIR}/tes"
chmod +x "${INSTALL_DIR}/tes"

echo "✓ Installed tes to ${INSTALL_DIR}/tes"
"${INSTALL_DIR}/tes" env --install || true
echo "✓ Configured shell prompt integration (restart your terminal or reload your shell profile)."
