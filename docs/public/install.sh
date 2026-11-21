#!/bin/sh
# Cross-platform installer for Tessera (`tes`) across macOS, Linux, FreeBSD, and OpenBSD.
set -eu

REPO="${TES_GITHUB_REPO:-tessera-vcs/tessera}"
VERSION="${TES_VERSION:-latest}"
INSTALL_DIR="${TES_INSTALL_DIR:-$HOME/.local/bin}"

if [ "$VERSION" != "latest" ]; then
  case "$VERSION" in
    v*) ;;
    *) VERSION="v${VERSION}" ;;
  esac
fi

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

has_bsd_ftp() {
  case "$(uname -s)" in
    FreeBSD|OpenBSD|NetBSD|Darwin)
      command -v ftp >/dev/null 2>&1
      ;;
    *)
      return 1
      ;;
  esac
}

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
    status_file="${dest}.status"
    err_file="${dest}.err"

    while [ "$count" -lt "$retries" ]; do
      count=$((count + 1))
      rm -f "$dest" "$status_file" "$err_file"

      if [ -n "$size_str" ]; then
        printf "  ${PURPLE}↓${RESET} %s... (0%% of %s)" "$label" "$size_str"
      else
        printf "  ${PURPLE}↓${RESET} %s... (0%%)" "$label"
      fi

      (
        rc=0
        curl -fL -# "$url" -o "$dest" 2>&1 || rc=$?
        printf '%s\n' "$rc" > "$status_file"
      ) | awk -v RS="$CR" -v errfile="$err_file" '
        {
          n = split($0, parts, "\n")
          for (i = 1; i <= n; i++) {
            line = parts[i]
            if (match(line, /[0-9]+(\.[0-9]+)?%/)) {
              print substr(line, RSTART, RLENGTH)
              system("")
            } else {
              gsub(/^[ \t]+|[ \t]+$/, "", line)
              if (length(line) > 0) {
                print line >> errfile
              }
            }
          }
        }
      ' | while IFS= read -r pct; do
        if [ -n "$size_str" ]; then
          printf "\r  ${PURPLE}↓${RESET} %s... (%s of %s)\033[K" "$label" "$pct" "$size_str"
        else
          printf "\r  ${PURPLE}↓${RESET} %s... (%s)\033[K" "$label" "$pct"
        fi
      done

      curl_rc="$(cat "$status_file" 2>/dev/null || true)"
      case "$curl_rc" in
        ''|*[!0-9]*) curl_rc=1 ;;
      esac

      if [ "$curl_rc" -eq 0 ] && [ -s "$dest" ]; then
        rm -f "$status_file" "$err_file"

        if [ -n "$size_str" ]; then
          printf "\r  ${GREEN}✔${RESET} %s (100%% of %s)\033[K\n" "$label" "$size_str"
        else
          printf "\r  ${GREEN}✔${RESET} %s (100%%)\033[K\n" "$label"
        fi

        return 0
      fi

      rm -f "$dest"

      if [ "$count" -lt "$retries" ]; then
        printf "\r  ${YELLOW}⚠${RESET} %s (retrying %d/%d in %ds...)\033[K\n" "$label" "$count" "$retries" "$delay"
        sleep "$delay"
      fi
    done

    printf "\r  ${RED}✖${RESET} %s (failed after %d attempts)\033[K\n" "$label" "$retries" >&2

    if [ -s "$err_file" ]; then
      while IFS= read -r err_line; do
        error "$err_line"
      done < "$err_file"
    fi

    rm -f "$status_file" "$err_file"
    return 1
  elif command -v wget >/dev/null 2>&1; then
    rm -f "$dest"
    printf "  ${PURPLE}↓${RESET} %s...\n" "$label"
    wget --show-progress -qO "$dest" "$url" 2>/dev/null || wget -qO "$dest" "$url"
    success "$label"
    return 0
  elif has_bsd_ftp; then
    rm -f "$dest"
    printf "  ${PURPLE}↓${RESET} %s...\n" "$label"
    ftp -o "$dest" "$url"
    success "$label"
    return 0
  else
    error "curl or wget is required (or BSD ftp on BSD/macOS)."
    return 1
  fi
}

download_quiet() {
  url="$1"
  dest="$2"
  rm -f "$dest"

  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$dest"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$dest" "$url"
  elif has_bsd_ftp; then
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
    if [ "$RUST_ARCH" = "aarch64" ]; then
      error "Prebuilt binaries for aarch64 FreeBSD/OpenBSD are not yet published; build from source with cargo install"
      exit 1
    fi

    TARGET="${RUST_ARCH}-unknown-freebsd"
    ;;
  OpenBSD)
    if [ "$RUST_ARCH" = "aarch64" ]; then
      error "Prebuilt binaries for aarch64 FreeBSD/OpenBSD are not yet published; build from source with cargo install"
      exit 1
    fi

    TARGET="${RUST_ARCH}-unknown-openbsd"
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
STAGE_BIN=""

cleanup() {
  if [ -n "${STAGE_BIN:-}" ]; then
    rm -f "$STAGE_BIN"
  fi

  if [ -n "${TMP_DIR:-}" ]; then
    rm -rf "$TMP_DIR"
  fi
}

trap cleanup EXIT
trap 'cleanup; trap - INT; kill -INT $$' INT
trap 'cleanup; trap - TERM; kill -TERM $$' TERM
trap 'cleanup; trap - HUP; kill -HUP $$' HUP

step "2" "Downloading & verifying archive..."

download_with_progress "${BASE_URL}/${ARCHIVE}" "${TMP_DIR}/${ARCHIVE}" "Downloading tes"

if [ "${TES_SKIP_CHECKSUM:-0}" != "1" ]; then
  if ! download_quiet "${BASE_URL}/${ARCHIVE}.sha256" "${TMP_DIR}/${ARCHIVE}.sha256" || [ ! -s "${TMP_DIR}/${ARCHIVE}.sha256" ]; then
    error "Missing or empty SHA-256 checksum file for ${ARCHIVE}"
    exit 1
  fi

  EXPECTED_HASH="$(awk '{print $1; exit}' "${TMP_DIR}/${ARCHIVE}.sha256" | tr -d '\r[:space:]' | tr '[:upper:]' '[:lower:]')"
  case "$EXPECTED_HASH" in
    *[!0-9a-f]*)
      error "Invalid SHA-256 checksum format in ${ARCHIVE}.sha256"
      exit 1
      ;;
  esac

  if [ "${#EXPECTED_HASH}" -ne 64 ]; then
    error "Invalid SHA-256 checksum length in ${ARCHIVE}.sha256 (expected 64 hex chars, got ${#EXPECTED_HASH})"
    exit 1
  fi

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

  ACTUAL_HASH="$(printf '%s' "$ACTUAL_HASH" | tr -d '\r[:space:]' | tr '[:upper:]' '[:lower:]')"

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

STAGE_BIN="${INSTALL_DIR}/.tes.tmp.$$"
cp "${TMP_DIR}/tes" "${STAGE_BIN}"
chmod 755 "${STAGE_BIN}"
mv -f "${STAGE_BIN}" "${INSTALL_DIR}/tes"
STAGE_BIN=""

success "Installed tes to ${INSTALL_DIR}/tes"

case ":${PATH}:" in
  *":${INSTALL_DIR}:"*)
    ;;
  *)
    warn "${INSTALL_DIR} is not in your PATH. Add it with: export PATH=\"${INSTALL_DIR}:\$PATH\""
    ;;
esac

info "Run 'tes env --install' to configure shell integration."
