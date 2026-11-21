# tes upgrade

Check for or install the latest Tessera CLI release directly in place (also available as `tes update`, `tes --upgrade`, and `tes --update`).

## Usage

```bash:no-line-numbers
tes upgrade [options]
tes update [options]
tes --upgrade [options]
tes --update [options]
```

## Options

- `--check` - Check whether a newer release is available without modifying the installed binary.
- `--version <VERSION>` (`--tag`) - Install a specific release version or tag (for example, `0.0.3` or `v0.0.3`).
- `-f, --force` - Reinstall even if the installed version is already up to date or newer than the target release.
- `--json` - Emit structured JSON status output.

## Description

`tes upgrade` resolves the latest GitHub Release for your platform (`aarch64`/`x86_64` on macOS, Linux, and Windows; `x86_64` on FreeBSD and OpenBSD), downloads the matching release archive and `.sha256` checksum, verifies the archive's SHA-256 digest before extracting, and atomically replaces the running `tes` binary:

- **macOS, Linux, and BSD**: stages the extracted binary next to the current executable with mode `0755` and atomically renames it into place (following symlinks to update the canonical target).
- **Windows**: renames the running `tes.exe` aside to `tes.exe.old`, moves the verified replacement into place, and cleans up `tes.exe.old` automatically on the next invocation.

## Examples

```bash:no-line-numbers
# check if a newer release is available
tes upgrade --check

# upgrade to the latest release
tes upgrade

# equivalent shorthand aliases
tes update
tes --upgrade
tes --update

# install or reinstall a specific version
tes upgrade --version 0.0.3 --force
```
