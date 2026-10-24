# Installation

This guide covers installing the `tes` CLI, enabling shell prompt integration, and configuring your AI coding editors.

## Requirements

Tessera is lightweight and self-contained:

- **OS**: macOS (APFS), Linux (Btrfs / XFS / bcachefs / ext4), FreeBSD, OpenBSD, or Windows (WSL2 and native Windows with PowerShell / Windows Terminal)
- **Git**: Standard Git 2.23+

> [!INFO]
> On macOS (APFS) and Linux (Btrfs / XFS / bcachefs), Tessera uses native Copy-on-Write filesystem clones (`clonefile` and `FICLONE`) to duplicate `node_modules` and `target` directories in milliseconds without using extra disk space. On ext4, FreeBSD, OpenBSD, and Windows, Tessera automatically falls back to portable recursive sandbox cloning.

## Installing the CLI

### macOS, Linux, and BSD

Install `tes` on macOS (Apple Silicon & Intel), Linux (`x86_64` & `ARM64`, glibc & musl), FreeBSD, or OpenBSD:

```bash:no-line-numbers
curl -fsSL https://tessera.getlunaris.dev/install.sh | sh
```

### Windows (PowerShell)

Install `tes.exe` on Windows (`x86_64` and `ARM64`) using PowerShell:

```powershell:no-line-numbers
irm https://tessera.getlunaris.dev/install.ps1 | iex
```

The Windows installer verifies the SHA-256 checksum, extracts `tes.exe` into `$HOME\.local\bin`, and adds it to your user `Path`.

### Prebuilt Binaries

You can also download prebuilt archives directly from GitHub Releases:

| Platform                           | Architecture                            | Release Archive                                                                  |
| ---------------------------------- | --------------------------------------- | -------------------------------------------------------------------------------- |
| **macOS**                          | Apple Silicon (`arm64`)                 | [`tes-aarch64-apple-darwin.tar.gz`](https://github.com/tessera-vcs/tessera/releases/latest/download/tes-aarch64-apple-darwin.tar.gz) |
| **macOS**                          | Intel (`x86_64`)                        | [`tes-x86_64-apple-darwin.tar.gz`](https://github.com/tessera-vcs/tessera/releases/latest/download/tes-x86_64-apple-darwin.tar.gz) |
| **Linux**                          | `x86_64` (static musl / glibc)          | [`tes-x86_64-unknown-linux-musl.tar.gz`](https://github.com/tessera-vcs/tessera/releases/latest/download/tes-x86_64-unknown-linux-musl.tar.gz) / [`tes-x86_64-unknown-linux-gnu.tar.gz`](https://github.com/tessera-vcs/tessera/releases/latest/download/tes-x86_64-unknown-linux-gnu.tar.gz) |
| **Linux**                          | `ARM64` (`aarch64` static musl / glibc) | [`tes-aarch64-unknown-linux-musl.tar.gz`](https://github.com/tessera-vcs/tessera/releases/latest/download/tes-aarch64-unknown-linux-musl.tar.gz) / [`tes-aarch64-unknown-linux-gnu.tar.gz`](https://github.com/tessera-vcs/tessera/releases/latest/download/tes-aarch64-unknown-linux-gnu.tar.gz) |
| **Windows**                        | `x86_64` (MSVC)                         | [`tes-x86_64-pc-windows-msvc.zip`](https://github.com/tessera-vcs/tessera/releases/latest/download/tes-x86_64-pc-windows-msvc.zip) |
| **Windows**                        | `ARM64` (MSVC)                          | [`tes-aarch64-pc-windows-msvc.zip`](https://github.com/tessera-vcs/tessera/releases/latest/download/tes-aarch64-pc-windows-msvc.zip) |
| **FreeBSD**                        | `x86_64`                                | [`tes-x86_64-unknown-freebsd.tar.gz`](https://github.com/tessera-vcs/tessera/releases/latest/download/tes-x86_64-unknown-freebsd.tar.gz) |
| **OpenBSD**                        | `x86_64`                                | [`tes-x86_64-unknown-openbsd.tar.gz`](https://github.com/tessera-vcs/tessera/releases/latest/download/tes-x86_64-unknown-openbsd.tar.gz) |
| **VS Code & VS Code-based IDEs**   | Universal `.vsix`                       | [`tessera-vscode.vsix`](https://github.com/tessera-vcs/tessera/releases/latest/download/tessera-vscode.vsix) |

Verify the installation:

```bash:no-line-numbers
tes --version
```

## Shell Integration

Each terminal tab can switch branches independently, so it helps to show the active branch in your prompt:

```text
~/code/my-app main (tes:feat/pricing) ❯
```

### Automatic Setup

Run `tes env --install` once to configure your shell profiles (`~/.zshrc`, `~/.bashrc`, `~/.bash_profile`, `~/.profile`, `~/.kshrc`, `~/.mkshrc`, `~/.tcshrc`, `~/.cshrc`, Fish's `config.fish`, and PowerShell's `$PROFILE` on Windows) automatically:

```bash:no-line-numbers
tes env --install
```

Open a new terminal tab (or reload your shell profile) to activate the prompt indicator and `<TAB>` auto-completion for branch names, commit SHAs, and checkpoints.

### Manual Shell Setup

If you prefer to configure your shell manually, add the line matching your shell:

**Zsh (`~/.zshrc`)** (Powerlevel10k, Oh My Zsh, Starship, and default Zsh)

```bash:no-line-numbers
eval "$(tes env --zsh)"
```

**Bash (`~/.bashrc` or `~/.bash_profile`)** (Bash 3.2+ and 5.x, Starship, Oh My Bash, and default Bash)

```bash:no-line-numbers
eval "$(tes env --bash)"
```

**Fish (`~/.config/fish/config.fish`)** (Tide, Starship, Hydro, and default Fish)

```bash:no-line-numbers
tes env --fish | source
```

**POSIX `sh` / `dash` / `ksh` / `mksh` / `oksh` / `yash` (`~/.profile`, `~/.kshrc`, or `~/.mkshrc`)**

```bash:no-line-numbers
eval "$(tes env --sh)"
```

**`tcsh` / BSD `csh` (`~/.tcshrc` or `~/.cshrc`)**

```bash:no-line-numbers
eval "`tes env --tcsh`"
```

**Nushell (`~/.config/nushell/config.nu`)**

```bash:no-line-numbers
tes env --nu | save -f ~/.config/nushell/tes.nu
source ~/.config/nushell/tes.nu
```

**Elvish (`~/.config/elvish/rc.elv`)**

```bash:no-line-numbers
tes env --elvish >> ~/.config/elvish/rc.elv
```

**PowerShell (`$PROFILE`)**

```powershell:no-line-numbers
Invoke-Expression (& tes env --pwsh)
```

## Initializing a Repository

Inside any project directory, run:

```bash:no-line-numbers
tes init
```

By default, `tes init` auto-detects which AI coding tools you use and generates their instruction files and hooks. To target a specific editor, pass `--harness`:

```bash:no-line-numbers
tes init --harness antigravity
tes init --harness claude
tes init --harness cursor
tes init --harness windsurf
tes init --harness kiro
tes init --harness opencode
tes init --harness codex
tes init --harness vscode
tes init --harness all
```

## Uninstalling Tessera

### 1. Remove Shell Integration

If you configured shell prompt integration (`tes env --install`), remove the `tes env` line from your shell profile:

- **Zsh (`~/.zshrc`)**: remove `eval "$(tes env --zsh 2>/dev/null)"`
- **Bash (`~/.bashrc`, `~/.bash_profile`)**: remove `eval "$(tes env --bash 2>/dev/null)"`
- **POSIX `sh` (`~/.profile`, `~/.kshrc`, `~/.mkshrc`)**: remove `eval "$(tes env --sh 2>/dev/null)"`
- **Fish (`~/.config/fish/config.fish`)**: remove `tes env --fish 2>/dev/null | source`
- **`tcsh` / `csh` (`~/.tcshrc`, `~/.cshrc`)**: remove `` eval "`tes env --tcsh`" ``
- **PowerShell (`$PROFILE`)**: remove `Invoke-Expression (& tes env --pwsh)`

### 2. Delete the CLI Binary

**macOS, Linux, and BSD:**

```bash:no-line-numbers
rm -f ~/.local/bin/tes
```

**Windows (PowerShell):**

```powershell:no-line-numbers
Remove-Item -Force "$HOME\.local\bin\tes.exe"
```

### 3. Clean Up Repository State (Optional)

Inside any repository where you ran `tes init`, Tessera stores its metadata and sandboxes in `.git/tessera` (and Git hooks that automatically no-op when `tes` is not installed). To remove the local Tessera state from a repository, delete `.git/tessera`.

---

## Next Steps

- [Your First Workflow](/getting-started/first-workflow) - Walk through a multi-branch workflow step by step
- [Guides](/guides/) - Explore in-depth guides for agents, terminals, and the IDE extension
