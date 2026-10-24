#!/bin/sh
# Cross-platform installer for Tessera (`tes`) across macOS, Linux, FreeBSD, OpenBSD, and NetBSD.
set -eu

REPO="${TES_GITHUB_REPO:-tessera-vcs/tessera}"
VERSION="${TES_VERSION:-latest}"
INSTALL_DIR="${TES_INSTALL_DIR:-$HOME/.local/bin}"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
PURPLE='\033[38;5;99m'
CYAN='\033[0;36m'
RESET='\033[0m'

info()    { printf "  ${PURPLE}ℹ${RESET} %s\n" "$1"; }
success() { printf "  ${GREEN}✔${RESET} %s\n" "$1"; }
warn()    { printf "  ${YELLOW}⚠${RESET} %s\n" "$1"; }
error()   { printf "  ${RED}✖${RESET} %s\n" "$1" >&2; }
step()    { printf "\n${CYAN}[%s/3]${RESET} %s\n" "$1" "$2"; }

download_with_progress() {
  url="$1"
  dest="$2"
  label="$3"
  CR="$(printf '\r')"

  size_bytes=""
  if command -v curl >/dev/null 2>&1; then
    size_bytes="$(curl -sLI "$url" 2>/dev/null | grep -i '^content-length:' | tail -n1 | awk '{print $2}' | tr -d '\r[:space:]' || true)"
  fi

  size_str=""
  case "$size_bytes" in
    ''|*[!0-9]*)
      ;;
    *)
      size_str="$(awk -v bytes="$size_bytes" 'BEGIN {
        if (bytes >= 1048576) {
          printf "%.1f MB", bytes / 1048576
        } else if (bytes >= 1024) {
          printf "%.1f KB", bytes / 1024
        } else if (bytes > 0) {
          printf "%d B", bytes
        }
      }')"
      ;;
  esac

  if command -v curl >/dev/null 2>&1; then
    retries=3
    delay=2
    count=0

    while [ "$count" -lt "$retries" ]; do
      count=$((count + 1))

      if [ -n "$size_str" ]; then
        printf "  ${PURPLE}⬇${RESET} %s... (0%% of %s)" "$label" "$size_str"
      else
        printf "  ${PURPLE}⬇${RESET} %s... (0%%)" "$label"
      fi

      curl -fL -# "$url" -o "$dest" 2>&1 | awk -v RS="$CR" '
        /([0-9]+(\.[0-9]+)?%)/ {
          match($0, /[0-9]+(\.[0-9]+)?%/)
          if (RSTART > 0) {
            print substr($0, RSTART, RLENGTH)
            system("")
          }
        }
      ' | while read -r pct; do
        if [ -n "$size_str" ]; then
          printf "\r  ${PURPLE}⬇${RESET} %s... (%s of %s)\033[K" "$label" "$pct" "$size_str"
        else
          printf "\r  ${PURPLE}⬇${RESET} %s... (%s)\033[K" "$label" "$pct"
        fi
      done

      if [ -s "$dest" ]; then
        if [ -n "$size_str" ]; then
          printf "\r  ${GREEN}✔${RESET} %s (100%% of %s)\033[K\n" "$label" "$size_str"
        else
          printf "\r  ${GREEN}✔${RESET} %s (100%%)\033[K\n" "$label"
        fi
        return 0
      fi

      printf "\r  ${YELLOW}⚠${RESET} %s (retrying %d/%d in %ds...)\033[K\n" "$label" "$count" "$retries" "$delay"
      sleep "$delay"
    done

    printf "\r  ${RED}✖${RESET} %s (failed after %d attempts)\033[K\n" "$label" "$retries" >&2
    return 1
  elif command -v wget >/dev/null 2>&1; then
    printf "  ${PURPLE}⬇${RESET} %s...\n" "$label"
    wget --show-progress -qO "$dest" "$url" 2>/dev/null || wget -qO "$dest" "$url"
    success "$label"
    return 0
  elif command -v ftp >/dev/null 2>&1; then
    printf "  ${PURPLE}⬇${RESET} %s...\n" "$label"
    ftp -o "$dest" "$url"
    success "$label"
    return 0
  else
    error "curl, wget, or ftp is required."
    return 1
  fi
}

download_quiet() {
  url="$1"
  dest="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$dest"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$dest" "$url"
  elif command -v ftp >/dev/null 2>&1; then
    ftp -V -o "$dest" "$url"
  else
    return 1
  fi
}

step "1" "Detecting platform & resolving release..."

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
    error "Unsupported architecture: $ARCH"
    exit 1
    ;;
esac

case "$OS" in
  Darwin)
    TARGET="${RUST_ARCH}-apple-darwin"
    ;;
  Linux)
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
    error "Unsupported operating system: $OS"
    exit 1
    ;;
esac

ARCHIVE="tes-${TARGET}.tar.gz"
if [ -n "${TES_BASE_URL:-}" ]; then
  BASE_URL="${TES_BASE_URL%/}"
  success "Target: ${TARGET}"
elif [ "$VERSION" = "latest" ]; then
  BASE_URL="https://github.com/${REPO}/releases/latest/download"
  success "Target: ${TARGET} (latest)"
else
  BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"
  success "Target: ${TARGET} (${VERSION})"
fi

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/tes-install.XXXXXX")"
trap 'rm -rf "$TMP_DIR"' EXIT

step "2" "Downloading & verifying archive..."

download_with_progress "${BASE_URL}/${ARCHIVE}" "${TMP_DIR}/${ARCHIVE}" "Downloading ${ARCHIVE}"

if [ "${TES_SKIP_CHECKSUM:-0}" != "1" ]; then
  if ! download_quiet "${BASE_URL}/${ARCHIVE}.sha256" "${TMP_DIR}/${ARCHIVE}.sha256" || [ ! -s "${TMP_DIR}/${ARCHIVE}.sha256" ]; then
    error "Missing or empty SHA-256 checksum file for ${ARCHIVE}"
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
    error "No SHA-256 verification tool (sha256sum, shasum, sha256, or cksum) found."
    exit 1
  fi

  if [ "$ACTUAL_HASH" != "$EXPECTED_HASH" ]; then
    error "SHA-256 checksum mismatch for ${ARCHIVE} (expected ${EXPECTED_HASH}, got ${ACTUAL_HASH})"
    exit 1
  fi

  success "Verified SHA-256 checksum"
fi

step "3" "Installing binary..."

tar -xzf "${TMP_DIR}/${ARCHIVE}" -C "${TMP_DIR}"
mkdir -p "${INSTALL_DIR}"
cp "${TMP_DIR}/tes" "${INSTALL_DIR}/tes"
chmod +x "${INSTALL_DIR}/tes"

success "Installed tes to ${INSTALL_DIR}/tes"

case ":${PATH}:" in
  *":${INSTALL_DIR}:"*)
    ;;
  *)
    warn "${INSTALL_DIR} is not in your PATH. Add it with: export PATH=\"${INSTALL_DIR}:\$PATH\""
    ;;
esac

info "Run 'tes env --install' to configure shell integration."
