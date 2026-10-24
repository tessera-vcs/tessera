# tes env

Configure shell prompt integration and dynamic `<TAB>` completion.

## Usage

```bash:no-line-numbers
tes env [options]
```

## Options

- `--install` - Automatically append the shell integration to `~/.zshrc`, `~/.bashrc`, `~/.bash_profile`, `~/.profile`, `~/.kshrc`, `~/.mkshrc`, `~/.tcshrc`, `~/.cshrc`, `~/.config/fish/config.fish`, and PowerShell's `$PROFILE` on Windows (when present).
- `--zsh` - Output the Zsh integration script.
- `--bash` - Output the Bash (`3.2+` and `5.x`) integration script.
- `--fish` - Output the Fish integration script.
- `--sh` - Output the POSIX `sh` / `dash` / `ksh` / `mksh` / `oksh` / `yash` integration script.
- `--tcsh` / `--csh` - Output the `tcsh` / BSD `csh` integration script.
- `--nu` / `--nushell` - Output the Nushell (`nu`) integration script.
- `--elvish` - Output the Elvish integration script.
- `--pwsh` - Output the PowerShell (`pwsh`) integration script.

## Description

The `env` command configures your shell so each terminal tab displays its active Tessera branch (`(tes:<branch>)`) in the prompt, enables `<TAB>` completion for subcommands, branches, and checkpoints, and routes commands in switched terminals to the active branch's Copy-on-Write sandbox.

Even without `tes env` installed in your shell profile, Tessera automatically isolates terminal tabs across macOS, Linux, FreeBSD, OpenBSD, and Windows using terminal session environment variables (`TERM_SESSION_ID`, `ITERM_SESSION_ID`, `TMUX_PANE`, `ZELLIJ_PANE_ID`, `WEZTERM_PANE`, `KITTY_WINDOW_ID`, `GHOSTTY_SURFACE_ID`, `ALACRITTY_WINDOW_ID`, `GNOME_TERMINAL_SCREEN`, `KONSOLE_DBUS_SESSION`, `TILIX_ID`, `WT_SESSION`), POSIX session leader (`getsid(0)`), and Windows console/process tree tracking.

## Examples

```bash:no-line-numbers
# automatically configure your shell profiles
tes env --install

# evaluate manually in ~/.zshrc
eval "$(tes env --zsh)"

# evaluate manually in ~/.bashrc
eval "$(tes env --bash)"

# evaluate manually in ~/.config/fish/config.fish
tes env --fish | source

# evaluate manually in ~/.profile, ~/.kshrc, or ~/.mkshrc (sh, dash, ksh, mksh, oksh, yash)
eval "$(tes env --sh)"

# evaluate manually in ~/.tcshrc or ~/.cshrc
eval "`tes env --tcsh`"

# save and source in ~/.config/nushell/config.nu
tes env --nu | save -f ~/.config/nushell/tes.nu
source ~/.config/nushell/tes.nu

# append to ~/.config/elvish/rc.elv
tes env --elvish >> ~/.config/elvish/rc.elv

# evaluate manually in PowerShell ($PROFILE)
Invoke-Expression (& tes env --pwsh)
```
