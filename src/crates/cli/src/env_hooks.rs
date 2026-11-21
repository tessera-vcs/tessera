//! Multi-shell prompt, sandbox-hopping, tab-completion, and environment integration (`tes env`).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use agent::{Coordinator, detect_session_id};
use core::{atomic_write_file_relaxed, now_millis};
use serde_json::json;

use crate::output::CliContext;

/// Options parsed for `tes env`.
#[derive(Debug, Clone)]
pub struct EnvOptions {
    /// Emit Zsh plugin.
    pub zsh: bool,
    /// Emit Bash plugin.
    pub bash: bool,
    /// Emit Fish plugin.
    pub fish: bool,
    /// Emit POSIX `sh` plugin.
    pub sh: bool,
    /// Emit `tcsh` / `csh` plugin.
    pub tcsh: bool,
    /// Emit Nushell plugin.
    pub nu: bool,
    /// Emit Elvish plugin.
    pub elvish: bool,
    /// Emit PowerShell plugin.
    pub pwsh: bool,
    /// Install integration snippets into detected user shell profiles.
    pub install: bool,
    /// Optional explicit agent/session ID.
    pub agent: Option<String>,
}

/// Quotes `s` as a POSIX single-quoted string (`'...'`), escaping embedded single quotes as `'\''`.
fn posix_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Quotes `s` as a Fish single-quoted string (`'...'`), escaping `\` and `'`.
fn fish_single_quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('\'', "\\'");
    format!("'{escaped}'")
}

/// Quotes `s` as a `tcsh`/`csh` single-quoted string (`'...'`), escaping `'`, `!`, and stripping control characters.
fn tcsh_quote(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| !matches!(c, '\n' | '\r' | '\0'))
        .collect();
    let escaped = cleaned.replace('\'', "'\\''").replace('!', "\\!");
    format!("'{escaped}'")
}

/// Quotes `s` as an Elvish single-quoted string (`'...'`), escaping `'` by doubling (`''`).
fn elvish_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Returns the non-`TES_PORT` managed port environment variable names for unsetting.
fn non_tes_port_keys() -> Vec<&'static str> {
    workspace::POLYGLOT_PORT_ENV_KEYS
        .iter()
        .copied()
        .filter(|k| *k != "TES_PORT")
        .collect()
}

/// Emits portable POSIX shell code that removes `$TES_SHIM_BIN` from `$PATH` without bashisms.
fn posix_strip_shim_from_path() -> &'static str {
    r#"if [ -n "${TES_SHIM_BIN:-}" ]; then _tes_p=":${PATH:-}:"; while :; do case "$_tes_p" in *":${TES_SHIM_BIN}:"*) _tes_p="${_tes_p%%:"$TES_SHIM_BIN":*}:${_tes_p#*:"$TES_SHIM_BIN":}" ;; *) break ;; esac; done; _tes_p="${_tes_p#:}"; _tes_p="${_tes_p%:}"; export PATH="$_tes_p"; unset _tes_p; fi"#
}

/// Allocates or retrieves a lightweight per-session slot file under
/// `<common_git_dir>/tessera/session-slots/` without mutating the `OpLog`.
fn allocate_passive_session_slot(coord: &Coordinator, view: &core::View, session_id: &str) -> u16 {
    const SLOT_TTL_MS: u64 = 30 * 60 * 1000;

    let slots_dir = coord
        .git()
        .common_git_dir()
        .join("tessera")
        .join("session-slots");
    let safe_id = Coordinator::sanitize_branch_name(session_id);
    let slot_file = slots_dir.join(format!("{safe_id}.slot"));
    let now = now_millis();

    if let Ok(raw) = fs::read_to_string(&slot_file) {
        if let Some(first) = raw.lines().next() {
            if let Ok(existing) = first.trim().parse::<u16>() {
                if existing >= 1 {
                    let _ = atomic_write_file_relaxed(
                        &slot_file,
                        format!("{existing}\n{now}\n").as_bytes(),
                    );
                    return existing;
                }
            }
        }
    }

    let mut used_slots: BTreeSet<u16> = BTreeSet::new();

    for agent in view.agents.values() {
        if agent.slot >= 1 {
            used_slots.insert(agent.slot);
        }
    }

    if let Ok(entries) = fs::read_dir(&slots_dir) {
        let my_name = format!("{safe_id}.slot");

        for entry in entries.flatten() {
            let path = entry.path();
            let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            if fname == my_name || !fname.ends_with(".slot") {
                continue;
            }

            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            let mut lines = raw.lines();
            let Some(slot_num) = lines.next().and_then(|l| l.trim().parse::<u16>().ok()) else {
                continue;
            };
            let updated_ms = lines
                .next()
                .and_then(|l| l.trim().parse::<u64>().ok())
                .unwrap_or(0);

            if updated_ms > 0 && now.saturating_sub(updated_ms) > SLOT_TTL_MS {
                let _ = fs::remove_file(&path);
                continue;
            }

            used_slots.insert(slot_num);
        }
    }

    let assigned = (1u16..=u16::MAX)
        .find(|s| !used_slots.contains(s))
        .unwrap_or(1);

    let _ = fs::create_dir_all(&slots_dir);
    let _ = atomic_write_file_relaxed(&slot_file, format!("{assigned}\n{now}\n").as_bytes());

    assigned
}

/// Returns the Nushell integration script.
fn nu_plugin_script() -> &'static str {
    r#"# Tessera branch prompt integration for Nushell (nu)
def --env _tes_ensure_session_id [] {
    let cur = ($env.TES_SESSION_ID? | default "")
    if ($cur | is-empty) or ($cur | str starts-with "term-") {
        $env.TES_SESSION_ID = ("term-" + ($nu.pid | into string))
    }
    $env.TES_SHELL = "nu"
}

def --env _tes_clear_env [] {
    hide-env -i TES_BRANCH TES_PORT TES_REPO_ROOT TES_WORKSPACE TES_PROMPT TES_PORT_ISOLATION TES_SHIM_BIN
}

def --env _tes_sync_nu [] {
    _tes_ensure_session_id
    let info = (^tes env --json | complete)
    let raw = ($info.stdout | str trim)
    if $info.exit_code == 0 and ($raw | is-not-empty) {
        let data = ($raw | from json)
        if ($data | describe | str starts-with "record") and ("branch" in $data) {
            $env.TES_BRANCH = $data.branch
            $env.TES_PORT = ($data.port | into string)
            $env.TES_REPO_ROOT = $data.repo_root
            let ws = ($data.workspace? | default $data.repo_root)
            $env.TES_WORKSPACE = $ws
            $env.TES_PROMPT = ("(tes:" + $data.branch + ") ")
            return
        }
    }
    _tes_clear_env
}

export-env {
    _tes_sync_nu
}

export def --env --wrapped tes [...rest: string] {
    _tes_ensure_session_id
    ^tes ...$rest
    let code = $env.LAST_EXIT_CODE
    if ($rest | is-empty) or ($rest.0 != "env") {
        _tes_sync_nu
    }
    if $code != 0 {
        error make { msg: $"tes exited with status ($code)" }
    }
}
"#
}

/// Returns the Zsh integration plugin script with quote-aware command splitting,
/// wrapper/env-assignment skipping, strict path boundaries, and `.git` worktree pointer support.
fn zsh_plugin_script() -> &'static str {
    r#"# Tessera branch prompt & tab-completion integration for Zsh (Powerlevel10k, Oh My Zsh, Starship, standard Zsh)
_tes_init_session() {
  if [[ -n "${TES_SESSION_ID:-}" && "$TES_SESSION_ID" != term-* ]]; then
    return
  fi
  local cur_tty="${TTY:-}"
  if [[ -z "$cur_tty" ]]; then
    cur_tty="$(tty 2>/dev/null)"
  fi
  if [[ -z "$TES_SESSION_ID" || -z "$cur_tty" || "$cur_tty" == "not a tty" || "$_TES_SESSION_TTY" != "$cur_tty" ]]; then
    export _TES_SESSION_TTY="$cur_tty"
    export TES_SESSION_ID="term-$$"
  fi
}
_tes_init_session
setopt PROMPT_SUBST

tes_prompt_info() {
  if [ -n "$TES_BRANCH" ]; then
    echo "%F{cyan}(tes:${TES_BRANCH})%f "
  fi
}

# Powerlevel10k native segment support
prompt_tes() {
  if [[ -n "$TES_BRANCH" ]]; then
    p10k segment -f cyan -t "(tes:$TES_BRANCH)"
  fi
}

_tes_is_repo_prefix() {
  local phys="$1"
  local root="$2"
  [[ -n "$root" && ( "$phys" == "$root" || "$phys" == "$root/"* ) ]]
}

_tes_restore_repo_pwd() {
  if [[ -n "$_TES_PREV_PWD" ]]; then
    if [[ -d "$_TES_PREV_PWD" && ! -f "$_TES_PREV_PWD/.tessera/repo_root" ]]; then
      builtin cd -q "$_TES_PREV_PWD"
      unset _TES_PREV_PWD
      return
    fi
    unset _TES_PREV_PWD
  fi
  local check_dir="$PWD"
  local depth=0
  while [[ -n "$check_dir" && "$check_dir" != "/" && $depth -lt 8 ]]; do
    if [[ -e "$check_dir/.git" || "$check_dir" == */.tessera/tmp ]]; then
      return
    fi
    if [[ -f "$check_dir/.tessera/repo_root" && ! -L "$check_dir/.tessera/repo_root" ]]; then
      local root_dir="$(<"$check_dir/.tessera/repo_root")"
      if [[ -n "$root_dir" && -e "$root_dir/.git" ]]; then
        local rel_sub="${PWD#$check_dir}"
        if [[ -n "$rel_sub" && -d "${root_dir}${rel_sub}" ]]; then
          builtin cd -q "${root_dir}${rel_sub}"
        else
          builtin cd -q "$root_dir"
        fi
        return
      fi
    fi
    check_dir="${check_dir:h}"
    (( depth++ ))
  done
}

_tes_extract_cmd_word() {
  local -a toks
  toks=(${(z)1})
  local i=1 n=${#toks} tok skip_next=0
  while (( i <= n )); do
    tok="${toks[$i]}"
    (( i++ ))
    if (( skip_next )); then
      skip_next=0
      continue
    fi
    case "$tok" in
      *=*)
        if [[ "$tok" != '='* && "$tok" != '-'* ]]; then
          continue
        fi
        ;;
      command|builtin|noglob|nocorrect|exec|time|nohup)
        continue
        ;;
      env|nice|sudo)
        continue
        ;;
      -u|-C|-S|-n)
        skip_next=1
        continue
        ;;
      -*)
        continue
        ;;
    esac
    tok="${tok##*/}"
    REPLY="$tok"
    return 0
  done
  REPLY=""
  return 1
}

_tes_classify_line() {
  local raw="$1"
  local -a words
  words=(${(z)raw})
  local -a cur_cmd
  local saw_tes=0 need_ws_after_tes=0 need_ws_now=0
  local tok w
  _tes_flush_cmd() {
    (( ${#cur_cmd} == 0 )) && return
    _tes_extract_cmd_word "${(j: :)cur_cmd}"
    w="$REPLY"
    cur_cmd=()
    [[ -z "$w" ]] && return
    case "$w" in
      tes)
        saw_tes=1
        need_ws_after_tes=0
        ;;
      git|cd|pushd|popd|pwd|dirs|agy|agy-*|antigravity|antigravity-*|code|code-insiders|codium|cursor|cursor-agent|windsurf|zed|kiro|kiro-cli|claude|claude-code|codex|opencode|aider|aider-chat|gemini|gemini-cli|copilot|amp|goose|crush|auggie|augment|droid|cline|roo|continue|openhands|plandex|mentat|q|subl|idea|webstorm|pycharm|goland|rustrover|clion|fleet|open|eval|source|.|export|unset|alias|unalias|which|type|clear|reset|exit|logout|jobs|fg|bg|disown|wait|kill|killall|pkill|ps|lsof|top|htop|_tes_*|tes_prompt_info*)
        ;;
      *)
        if (( saw_tes )); then
          need_ws_after_tes=1
        else
          need_ws_now=1
        fi
        ;;
    esac
  }
  for tok in "${words[@]}"; do
    case "$tok" in
      ";"|";;"|"&&"|"||"|"|"|"|&"|"&"|$'\n')
        _tes_flush_cmd
        ;;
      *)
        cur_cmd+=("$tok")
        ;;
    esac
  done
  _tes_flush_cmd
  _TES_CLASSIFY_AFTER=$need_ws_after_tes
  _TES_CLASSIFY_NOW=$need_ws_now
}

_tes_preexec() {
  unset _TES_CHAINED_AFTER_TES
  _tes_restore_repo_pwd
  _tes_classify_line "$1"
  if (( _TES_CLASSIFY_AFTER )); then
    _TES_CHAINED_AFTER_TES=1
    return
  fi
  if (( ! _TES_CLASSIFY_NOW )); then
    return
  fi
  local phys_pwd="${PWD:A}"
  if [[ -n "$TES_WORKSPACE" && -n "$TES_REPO_ROOT" && "$TES_WORKSPACE" != "$TES_REPO_ROOT" && -e "$TES_REPO_ROOT/.git" ]] && _tes_is_repo_prefix "$phys_pwd" "$TES_REPO_ROOT"; then
    local env_out
    if command -v tes >/dev/null 2>&1 && env_out="$(_TES_IN_PREEXEC=1 command tes env 2>/dev/null)" && [[ -n "$env_out" ]]; then
      eval "$env_out"
    fi
    if [[ -n "$TES_WORKSPACE" && -d "$TES_WORKSPACE" && "$TES_WORKSPACE" != "$TES_REPO_ROOT" ]]; then
      _TES_PREV_PWD="$PWD"
      local rel_sub="${phys_pwd#$TES_REPO_ROOT}"
      if [[ -d "${TES_WORKSPACE}${rel_sub}" ]]; then
        builtin cd -q "${TES_WORKSPACE}${rel_sub}"
      else
        builtin cd -q "$TES_WORKSPACE"
      fi
    fi
  fi
}

_tes_precmd() {
  _tes_init_session
  unset _TES_CHAINED_AFTER_TES
  _tes_restore_repo_pwd
  local env_out
  if command -v tes >/dev/null 2>&1 && env_out="$(command tes env 2>/dev/null)" && [[ -n "$env_out" ]]; then
    eval "$env_out"
  else
    if [[ -n "${TES_SHIM_BIN:-}" ]]; then
      local _tes_p=":${PATH:-}:"
      while [[ "$_tes_p" == *":${TES_SHIM_BIN}:"* ]]; do
        _tes_p="${_tes_p%%:"$TES_SHIM_BIN":*}:${_tes_p#*:"$TES_SHIM_BIN":}"
      done
      _tes_p="${_tes_p#:}"
      _tes_p="${_tes_p%:}"
      export PATH="$_tes_p"
    fi
    unset TES_BRANCH TES_PORT TES_WORKSPACE TES_REPO_ROOT TES_PORT_ISOLATION TES_SHIM_BIN
  fi
}

_tes_ensure_prompt() {
  if (( ! $+functions[p10k] )); then
    if [[ "$PROMPT" != *"tes_prompt_info"* && "$RPROMPT" != *"tes_prompt_info"* ]]; then
      PROMPT='$(tes_prompt_info)'"$PROMPT"
    fi
  fi
}

tes() {
  _tes_init_session
  local chained="$_TES_CHAINED_AFTER_TES"
  _tes_restore_repo_pwd
  command tes "$@"
  local ret=$?
  if [[ "$1" != "env" ]]; then
    _tes_precmd
    if [[ -n "$chained" ]]; then
      _TES_CHAINED_AFTER_TES=1
      local phys_pwd="${PWD:A}"
      if [[ -n "$TES_WORKSPACE" && -d "$TES_WORKSPACE" && -n "$TES_REPO_ROOT" && "$TES_WORKSPACE" != "$TES_REPO_ROOT" && -e "$TES_REPO_ROOT/.git" ]] && _tes_is_repo_prefix "$phys_pwd" "$TES_REPO_ROOT"; then
        _TES_PREV_PWD="$PWD"
        local rel_sub="${phys_pwd#$TES_REPO_ROOT}"
        if [[ -d "${TES_WORKSPACE}${rel_sub}" ]]; then
          builtin cd -q "${TES_WORKSPACE}${rel_sub}"
        else
          builtin cd -q "$TES_WORKSPACE"
        fi
      fi
    fi
  fi
  return $ret
}

preexec_functions=(_tes_preexec ${preexec_functions:#_tes_preexec})
precmd_functions=(_tes_precmd ${precmd_functions:#_tes_precmd})
precmd_functions=(${precmd_functions:#_tes_ensure_prompt} _tes_ensure_prompt)

if (( $+functions[p10k] )); then
  if (( ! ${POWERLEVEL9K_LEFT_PROMPT_ELEMENTS[(I)tes]} )); then
    local vcs_idx=${POWERLEVEL9K_LEFT_PROMPT_ELEMENTS[(I)vcs]}
    if (( vcs_idx > 0 )); then
      POWERLEVEL9K_LEFT_PROMPT_ELEMENTS=(
        "${POWERLEVEL9K_LEFT_PROMPT_ELEMENTS[@]:0:$vcs_idx}"
        tes
        "${POWERLEVEL9K_LEFT_PROMPT_ELEMENTS[@]:$vcs_idx}"
      )
    else
      POWERLEVEL9K_LEFT_PROMPT_ELEMENTS+=(tes)
    fi
    p10k reload
  fi
else
  _tes_ensure_prompt
fi

_tes_completion() {
  local -a completions
  local IFS=$'\n'
  local cword=$(( CURRENT - 1 ))
  completions=($(command tes complete --shell zsh --cword "$cword" -- "${words[@]}" 2>/dev/null))
  if (( ${#completions[@]} > 0 )); then
    _describe -t tes 'tes' completions
  else
    _files
  fi
}
if (( $+functions[compdef] )); then
  compdef _tes_completion tes
fi

_tes_precmd
"#
}

/// Returns the Bash integration plugin script preserving array `PROMPT_COMMAND`,
/// prior `DEBUG` traps, explicit non-`term-` session IDs, quote-aware command parsing,
/// and strict repository path boundaries.
fn bash_plugin_script() -> &'static str {
    r#"# Tessera branch prompt & tab-completion integration for Bash
_tes_init_session() {
  if [ -z "${TES_SESSION_ID:-}" ] || [[ "$TES_SESSION_ID" == term-* ]]; then
    export TES_SESSION_ID="term-$$"
  fi
}
_tes_init_session

tes_prompt_info() {
  if [ -n "$TES_BRANCH" ]; then
    printf "\001\033[36m\002(tes:%s)\001\033[0m\002 " "$TES_BRANCH"
  fi
}

_tes_restore_repo_pwd() {
  if [ -n "${_TES_PREV_PWD:-}" ]; then
    if [ -d "$_TES_PREV_PWD" ] && [ ! -f "$_TES_PREV_PWD/.tessera/repo_root" ]; then
      builtin cd "$_TES_PREV_PWD"
      unset _TES_PREV_PWD
      return
    fi
    unset _TES_PREV_PWD
  fi
  local check_dir="$PWD"
  local depth=0
  while [ -n "$check_dir" ] && [ "$check_dir" != "/" ] && [ "$depth" -lt 8 ]; do
    if [ -e "$check_dir/.git" ]; then
      return
    fi
    case "$check_dir" in
      */.tessera/tmp) return ;;
    esac
    if [ -f "$check_dir/.tessera/repo_root" ] && [ ! -L "$check_dir/.tessera/repo_root" ]; then
      local root_dir
      root_dir="$(cat "$check_dir/.tessera/repo_root" 2>/dev/null)"
      if [ -n "$root_dir" ] && [ -e "$root_dir/.git" ]; then
        local rel_sub="${PWD#$check_dir}"
        if [ -n "$rel_sub" ] && [ -d "${root_dir}${rel_sub}" ]; then
          builtin cd "${root_dir}${rel_sub}"
        else
          builtin cd "$root_dir"
        fi
        return
      fi
    fi
    check_dir="${check_dir%/*}"
    depth=$((depth + 1))
  done
}

_tes_first_exec_word() {
  local s="$1"
  local i=0 len=${#s} ch in_sq=0 in_dq=0 esc=0 tok="" skip_next=0
  local -a toks=()
  while (( i < len )); do
    ch="${s:i:1}"
    (( i++ ))
    if (( esc )); then
      tok+="$ch"
      esc=0
      continue
    fi
    if (( in_sq )); then
      if [[ "$ch" == "'" ]]; then in_sq=0; else tok+="$ch"; fi
      continue
    fi
    if (( in_dq )); then
      if [[ "$ch" == '"' ]]; then
        in_dq=0
      elif [[ "$ch" == '\' ]]; then
        esc=1
      else
        tok+="$ch"
      fi
      continue
    fi
    case "$ch" in
      "'") in_sq=1 ;;
      '"') in_dq=1 ;;
      '\') esc=1 ;;
      ';'|'&'|'|'|$'\n')
        [[ -n "$tok" ]] && toks+=("$tok")
        break
        ;;
      [[:space:]])
        if [[ -n "$tok" ]]; then
          toks+=("$tok")
          tok=""
        fi
        ;;
      *) tok+="$ch" ;;
    esac
  done
  [[ -n "$tok" ]] && toks+=("$tok")
  for tok in "${toks[@]}"; do
    if (( skip_next )); then
      skip_next=0
      continue
    fi
    case "$tok" in
      *=*)
        if [[ "$tok" != '='* && "$tok" != '-'* ]]; then
          continue
        fi
        ;;
      command|builtin|exec|time|nohup|env|nice|sudo)
        continue
        ;;
      -u|-C|-S|-n)
        skip_next=1
        continue
        ;;
      -*)
        continue
        ;;
    esac
    printf '%s' "${tok##*/}"
    return
  done
}

_tes_preexec() {
  if [ -n "${_TES_PREV_DEBUG_TRAP:-}" ]; then
    eval "$_TES_PREV_DEBUG_TRAP"
  fi
  if [ -n "${COMP_LINE:-}" ]; then
    return
  fi
  _tes_restore_repo_pwd
  local first_word
  first_word="$(_tes_first_exec_word "$1")"
  [[ -z "$first_word" ]] && return
  case "$first_word" in
    tes|git|cd|pushd|popd|pwd|dirs|agy|agy-*|antigravity|antigravity-*|code|code-insiders|codium|cursor|cursor-agent|windsurf|zed|kiro|kiro-cli|claude|claude-code|codex|opencode|aider|aider-chat|gemini|gemini-cli|copilot|amp|goose|crush|auggie|augment|droid|cline|roo|continue|openhands|plandex|mentat|q|subl|idea|webstorm|pycharm|goland|rustrover|clion|fleet|open|eval|source|.|export|unset|alias|unalias|which|type|clear|reset|exit|logout|jobs|fg|bg|disown|wait|kill|killall|pkill|ps|lsof|top|htop|_tes_*|tes_prompt_info*) return ;;
  esac
  if [ -n "${TES_WORKSPACE:-}" ] && [ -n "${TES_REPO_ROOT:-}" ] && [ "$TES_WORKSPACE" != "$TES_REPO_ROOT" ] && [ -e "$TES_REPO_ROOT/.git" ]; then
    local phys_pwd
    phys_pwd="$(builtin pwd -P 2>/dev/null || printf '%s' "$PWD")"
    if [ "$phys_pwd" = "$TES_REPO_ROOT" ] || [[ "$phys_pwd" == "$TES_REPO_ROOT/"* ]]; then
      local env_out
      if command -v tes >/dev/null 2>&1 && env_out="$(_TES_IN_PREEXEC=1 command tes env 2>/dev/null)" && [ -n "$env_out" ]; then
        eval "$env_out"
      fi
      if [ -n "${TES_WORKSPACE:-}" ] && [ -d "$TES_WORKSPACE" ] && [ "$TES_WORKSPACE" != "$TES_REPO_ROOT" ]; then
        _TES_PREV_PWD="$PWD"
        local rel_sub="${phys_pwd#$TES_REPO_ROOT}"
        if [ -d "${TES_WORKSPACE}${rel_sub}" ]; then
          builtin cd "${TES_WORKSPACE}${rel_sub}"
        else
          builtin cd "$TES_WORKSPACE"
        fi
      fi
    fi
  fi
}

_tes_prompt_command() {
  _tes_init_session
  _tes_restore_repo_pwd
  local env_out
  if command -v tes >/dev/null 2>&1 && env_out="$(command tes env 2>/dev/null)" && [ -n "$env_out" ]; then
    eval "$env_out"
  else
    if [ -n "${TES_SHIM_BIN:-}" ]; then
      local _tes_p=":${PATH:-}:"
      while [[ "$_tes_p" == *":${TES_SHIM_BIN}:"* ]]; do
        _tes_p="${_tes_p%%:"$TES_SHIM_BIN":*}:${_tes_p#*:"$TES_SHIM_BIN":}"
      done
      _tes_p="${_tes_p#:}"
      _tes_p="${_tes_p%:}"
      export PATH="$_tes_p"
    fi
    unset TES_BRANCH TES_PORT TES_WORKSPACE TES_REPO_ROOT TES_PORT_ISOLATION TES_SHIM_BIN
  fi
  if [[ "$PS1" != *"tes_prompt_info"* ]]; then
    PS1='$(tes_prompt_info)'"$PS1"
  fi
}

tes() {
  _tes_init_session
  _tes_restore_repo_pwd
  command tes "$@"
  local ret=$?
  if [ "${1:-}" != "env" ]; then
    _tes_prompt_command
  fi
  return $ret
}

if [[ "$(declare -p PROMPT_COMMAND 2>/dev/null)" == "declare -a"* ]]; then
  _tes_has_pc=0
  for _tes_pc_item in "${PROMPT_COMMAND[@]}"; do
    [[ "$_tes_pc_item" == "_tes_prompt_command" ]] && _tes_has_pc=1
  done
  (( _tes_has_pc )) || PROMPT_COMMAND+=(_tes_prompt_command)
  unset _tes_has_pc _tes_pc_item
elif [[ ";${PROMPT_COMMAND:-};" != *";_tes_prompt_command;"* ]]; then
  PROMPT_COMMAND="${PROMPT_COMMAND:+$PROMPT_COMMAND;}_tes_prompt_command"
fi

_tes_existing_debug="$(trap -p DEBUG 2>/dev/null)"
if [[ -n "$_tes_existing_debug" && "$_tes_existing_debug" != *"_tes_preexec"* ]]; then
  _tes_existing_debug="${_tes_existing_debug#trap -- \'}"
  _tes_existing_debug="${_tes_existing_debug%\' DEBUG}"
  _TES_PREV_DEBUG_TRAP="$_tes_existing_debug"
fi
unset _tes_existing_debug
trap '_tes_preexec "$BASH_COMMAND"' DEBUG

_tes_bash_completion() {
  local IFS=$'\n'
  COMPREPLY=($(command tes complete --shell bash --cword "$COMP_CWORD" -- "${COMP_WORDS[@]}" 2>/dev/null))
}
complete -o default -F _tes_bash_completion tes

_tes_prompt_command
"#
}

/// Returns the Fish shell integration plugin script.
fn fish_plugin_script() -> &'static str {
    r#"# Tessera branch prompt & tab-completion integration for Fish
function _tes_init_session
    if not set -q TES_SESSION_ID; or string match -q "term-*" -- "$TES_SESSION_ID"
        set -gx TES_SESSION_ID "term-$fish_pid"
    end
    set -gx TES_SHELL "fish"
end
_tes_init_session

function _tes_restore_repo_pwd
    if set -q _TES_PREV_PWD
        if test -d "$_TES_PREV_PWD"; and not test -f "$_TES_PREV_PWD/.tessera/repo_root"
            builtin cd "$_TES_PREV_PWD"
            set -e _TES_PREV_PWD
            return
        end
        set -e _TES_PREV_PWD
    end
    set -l check_dir "$PWD"
    set -l depth 0
    while test -n "$check_dir"; and test "$check_dir" != "/"; and test $depth -lt 8
        if test -e "$check_dir/.git"; or string match -q "*/.tessera/tmp" -- "$check_dir"
            return
        end
        if test -f "$check_dir/.tessera/repo_root"; and not test -L "$check_dir/.tessera/repo_root"
            set -l root_dir (cat "$check_dir/.tessera/repo_root" 2>/dev/null)
            if test -n "$root_dir"; and test -e "$root_dir/.git"
                set -l rel_sub (string replace -- "$check_dir" "" "$PWD")
                if test -n "$rel_sub"; and test -d "$root_dir$rel_sub"
                    builtin cd "$root_dir$rel_sub"
                else
                    builtin cd "$root_dir"
                end
                return
            end
        end
        set check_dir (string replace -r '/[^/]*$' '' -- "$check_dir")
        set depth (math $depth + 1)
    end
end

function _tes_preexec --on-event fish_preexec
    _tes_restore_repo_pwd
    set -l cleaned (string replace -r '^\s*([A-Za-z_][A-Za-z0-9_]*=("[^"]*"|\'[^\']*\'|[^\s]*)\s+)*' '' -- "$argv[1]")
    set cleaned (string replace -r '^((command|builtin|exec|time|nohup|env|nice|sudo)\s+(-[^\s]+\s+)*)+' '' -- "$cleaned")
    if string match -qr '^([^\s;&|]*/)?(tes|git|cd|pushd|popd|pwd|dirs|agy|agy-[^\s]*|antigravity|antigravity-[^\s]*|code|code-insiders|codium|cursor|cursor-agent|windsurf|zed|kiro|kiro-cli|claude|claude-code|codex|opencode|aider|aider-chat|gemini|gemini-cli|copilot|amp|goose|crush|auggie|augment|droid|cline|roo|continue|openhands|plandex|mentat|q|subl|idea|webstorm|pycharm|goland|rustrover|clion|fleet|open|eval|source|export|set|unset|alias|which|type|clear|reset|exit|logout|jobs|fg|bg|disown|wait|kill|killall|pkill|ps|lsof|top|htop)(\s|$|;|&|\|)' -- "$cleaned"
        return
    end
    set -l phys_pwd (pwd -P 2>/dev/null; or echo "$PWD")
    if set -q TES_WORKSPACE; and set -q TES_REPO_ROOT; and test "$TES_WORKSPACE" != "$TES_REPO_ROOT"; and test -e "$TES_REPO_ROOT/.git"
        if test "$phys_pwd" = "$TES_REPO_ROOT"; or string match -q "$TES_REPO_ROOT/*" -- "$phys_pwd"
            if command -sq tes
                set -l env_out (env _TES_IN_PREEXEC=1 tes env 2>/dev/null)
                if test -n "$env_out"
                    printf "%s\n" $env_out | source
                end
            end
            if set -q TES_WORKSPACE; and test -d "$TES_WORKSPACE"; and test "$TES_WORKSPACE" != "$TES_REPO_ROOT"
                set -g _TES_PREV_PWD "$PWD"
                set -l rel_sub (string replace -- "$TES_REPO_ROOT" "" "$phys_pwd")
                if test -d "$TES_WORKSPACE$rel_sub"
                    builtin cd "$TES_WORKSPACE$rel_sub"
                else
                    builtin cd "$TES_WORKSPACE"
                end
            end
        end
    end
end

function _tes_sync_env --on-event fish_prompt --on-event fish_postexec
    _tes_init_session
    _tes_restore_repo_pwd
    if command -sq tes
        set -l env_out (command tes env 2>/dev/null)
        if test -n "$env_out"
            printf "%s\n" $env_out | source
            return
        end
    end
    if set -q TES_SHIM_BIN
        set -l _tes_idx (contains -i -- "$TES_SHIM_BIN" $PATH)
        while test -n "$_tes_idx"
            set -e PATH[$_tes_idx]
            set _tes_idx (contains -i -- "$TES_SHIM_BIN" $PATH)
        end
    end
    set -e TES_BRANCH TES_PORT TES_WORKSPACE TES_REPO_ROOT TES_PORT_ISOLATION TES_SHIM_BIN
end

function tes --wraps tes
    _tes_init_session
    _tes_restore_repo_pwd
    command tes $argv
    set -l ret $status
    if test "$argv[1]" != "env"
        _tes_sync_env
    end
    return $ret
end

if not functions -q _tes_orig_fish_prompt
    if functions -q fish_prompt
        functions -c fish_prompt _tes_orig_fish_prompt
        function fish_prompt
            if set -q TES_BRANCH
                set_color cyan
                echo -n "(tes:$TES_BRANCH) "
                set_color normal
            end
            _tes_orig_fish_prompt
        end
    end
end

function __tes_complete
    set -l cmd (commandline -opc)
    set -l cur (commandline -ct)
    set -l cword (count $cmd)
    set -a cmd $cur
    command tes complete --shell fish --cword $cword -- $cmd 2>/dev/null
end
complete -c tes -f -a "(__tes_complete)"

_tes_sync_env
"#
}

/// Returns the POSIX `sh` / `dash` / `ksh` / `mksh` / `oksh` / `yash` integration script.
fn sh_plugin_script() -> &'static str {
    r#"# Tessera branch prompt integration for POSIX sh / dash / ksh / mksh / oksh / yash
_tes_sh_init_session() {
  case "${TES_SESSION_ID:-}" in
    ""|term-*) export TES_SESSION_ID="term-$$" ;;
  esac
}
_tes_sh_init_session

_tes_sh_sync() {
  _tes_sh_init_session
  if [ -n "${_TES_PREV_PWD:-}" ]; then
    if [ -d "$_TES_PREV_PWD" ] && [ ! -f "$_TES_PREV_PWD/.tessera/repo_root" ]; then
      command cd "$_TES_PREV_PWD" || true
    fi
    unset _TES_PREV_PWD
  elif [ -f "$PWD/.tessera/repo_root" ] && [ ! -L "$PWD/.tessera/repo_root" ]; then
    _tes_root_dir="$(cat "$PWD/.tessera/repo_root" 2>/dev/null)"
    if [ -n "$_tes_root_dir" ] && [ -e "$_tes_root_dir/.git" ]; then
      command cd "$_tes_root_dir" || true
    fi
    unset _tes_root_dir
  fi
  if command -v tes >/dev/null 2>&1; then
    _tes_out="$(command tes env 2>/dev/null)"
    if [ -n "$_tes_out" ]; then
      eval "$_tes_out"
      TES_PROMPT="(tes:${TES_BRANCH}) "
      export TES_PROMPT
      return
    fi
  fi
  if [ -n "${TES_SHIM_BIN:-}" ]; then
    _tes_p=":${PATH:-}:"
    while :; do
      case "$_tes_p" in
        *":${TES_SHIM_BIN}:"*)
          _tes_p="${_tes_p%%:"$TES_SHIM_BIN":*}:${_tes_p#*:"$TES_SHIM_BIN":}"
          ;;
        *) break ;;
      esac
    done
    _tes_p="${_tes_p#:}"
    _tes_p="${_tes_p%:}"
    export PATH="$_tes_p"
    unset _tes_p
  fi
  unset TES_BRANCH TES_PORT TES_PROMPT TES_WORKSPACE TES_REPO_ROOT TES_PORT_ISOLATION TES_SHIM_BIN
}

tes() {
  _tes_sh_init_session
  command tes "$@"
  _tes_ret=$?
  if [ "${1:-}" != "env" ]; then
    _tes_sh_sync
  fi
  return $_tes_ret
}

cd() {
  command cd "$@"
  _tes_ret=$?
  _tes_sh_sync
  return $_tes_ret
}

case "${PS1:-}" in
  *TES_PROMPT*) ;;
  *) PS1='${TES_PROMPT:-}'"${PS1:-$ }" ;;
esac

_tes_sh_sync
"#
}

/// Returns the `tcsh` / `csh` integration script.
fn tcsh_plugin_script() -> &'static str {
    r#"if ( ! $?TES_SESSION_ID ) setenv TES_SESSION_ID "term-$$";
setenv TES_SHELL "tcsh";
alias _tes_sync 'eval "`\tes env`"';
alias tes '\tes \!* ; eval "`\tes env`"';
alias precmd 'eval "`\tes env`"';
eval "`\tes env`";
"#
}

/// Returns the Elvish integration script with safe nil `workspace` handling.
fn elvish_plugin_script() -> &'static str {
    r#"# Tessera branch prompt integration for Elvish
use str

fn _tes_init_session {
    if (or (not (has-env TES_SESSION_ID)) (str:has-prefix $E:TES_SESSION_ID "term-")) {
        set E:TES_SESSION_ID = "term-"$pid
    }
    set E:TES_SHELL = "elvish"
}
_tes_init_session

fn _tes_sync {
    _tes_init_session
    try {
        var raw = (e:tes env --json | slurp)
        if (eq (str:trim-space $raw) "") {
            unset-env TES_BRANCH
            unset-env TES_PORT
            unset-env TES_WORKSPACE
            unset-env TES_REPO_ROOT
            unset-env TES_PROMPT
            return
        }
        var data = (print $raw | from-json)
        set E:TES_BRANCH = $data[branch]
        set E:TES_PORT = (to-string $data[port])
        set E:TES_REPO_ROOT = $data[repo_root]
        if (and (has-key $data workspace) (not-eq $data[workspace] $nil)) {
            set E:TES_WORKSPACE = $data[workspace]
        } else {
            unset-env TES_WORKSPACE
        }
        set E:TES_PROMPT = "(tes:"$data[branch]") "
    } catch _ {
        unset-env TES_BRANCH
        unset-env TES_PORT
        unset-env TES_WORKSPACE
        unset-env TES_REPO_ROOT
        unset-env TES_PROMPT
    }
}

fn tes {|@a|
    _tes_init_session
    e:tes $@a
    _tes_sync
}

_tes_sync
"#
}

/// Returns the single-line PowerShell integration script (calling `tes complete` and preserving `$LASTEXITCODE`).
fn pwsh_plugin_script() -> &'static str {
    r#"if (-not $env:TES_SESSION_ID -or $env:TES_SESSION_ID -like "term-*") { $env:TES_SESSION_ID = "term-$PID" }; function global:_TesSyncEnv { if (-not $env:TES_SESSION_ID -or $env:TES_SESSION_ID -like "term-*") { $env:TES_SESSION_ID = "term-$PID" }; $tesCmd = Get-Command tes -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1; if ($tesCmd) { $raw = (& $tesCmd.Source env --json 2>$null | Out-String); if ($raw -and $raw.Trim()) { $info = ($raw | ConvertFrom-Json -ErrorAction SilentlyContinue); if ($info -and $info.branch) { $env:TES_BRANCH = $info.branch; $env:TES_PORT = "$($info.port)"; $env:TES_REPO_ROOT = $info.repo_root; if ($info.workspace) { $env:TES_WORKSPACE = $info.workspace } else { Remove-Item Env:\TES_WORKSPACE -ErrorAction SilentlyContinue }; return } } }; Remove-Item Env:\TES_BRANCH -ErrorAction SilentlyContinue; Remove-Item Env:\TES_PORT -ErrorAction SilentlyContinue; Remove-Item Env:\TES_WORKSPACE -ErrorAction SilentlyContinue; Remove-Item Env:\TES_REPO_ROOT -ErrorAction SilentlyContinue }; function global:tes { if (-not $env:TES_SESSION_ID -or $env:TES_SESSION_ID -like "term-*") { $env:TES_SESSION_ID = "term-$PID" }; $tesCmd = Get-Command tes -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1; $exitCode = 0; if ($tesCmd) { & $tesCmd.Source @args; $exitCode = $LASTEXITCODE }; if ($args.Count -eq 0 -or $args[0] -ne "env") { _TesSyncEnv }; $global:LASTEXITCODE = $exitCode }; Register-ArgumentCompleter -Native -CommandName tes -ScriptBlock { param($wordToComplete, $commandAst, $cursorPosition); $tokens = @($commandAst.CommandElements | ForEach-Object { $_.Extent.Text }); if ($wordToComplete -eq "" -and $cursorPosition -gt $commandAst.Extent.EndOffset) { $tokens += "" }; $cword = [Math]::Max(0, $tokens.Count - 1); $tesCmd = Get-Command tes -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1; if ($tesCmd) { & $tesCmd.Source complete --shell plain --cword $cword -- @tokens 2>$null | ForEach-Object { [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_) } } }; if (-not (Test-Path Function:\_TesOrigPrompt)) { Copy-Item Function:\prompt Function:\_TesOrigPrompt; function global:prompt { _TesSyncEnv; if ($env:TES_BRANCH) { Write-Host "(tes:$($env:TES_BRANCH)) " -NoNewline -ForegroundColor Cyan }; _TesOrigPrompt } }; _TesSyncEnv"#
}

/// Installs shell integration snippets atomically into detected shell profiles.
fn install_shell_profiles(json_out: bool) -> anyhow::Result<()> {
    let mut updated = Vec::new();

    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        let home_path = PathBuf::from(home);
        let shell_env = std::env::var("SHELL").unwrap_or_default();
        let is_bash_default = shell_env.ends_with("/bash") || shell_env == "bash";
        let is_zsh_default = shell_env.ends_with("/zsh") || shell_env == "zsh";

        let should_create_bashrc = home_path.join(".bashrc").exists()
            || (!cfg!(windows) && !cfg!(target_os = "macos") && is_bash_default);
        let should_create_zshrc = home_path.join(".zshrc").exists()
            || (!cfg!(windows) && (cfg!(target_os = "macos") || is_zsh_default));

        let nu_config_dir = if home_path.join(".config/nushell").exists() {
            Some(home_path.join(".config/nushell"))
        } else if home_path
            .join("Library/Application Support/nushell")
            .exists()
        {
            Some(home_path.join("Library/Application Support/nushell"))
        } else {
            std::env::var("APPDATA")
                .ok()
                .map(|a| PathBuf::from(a).join("nushell"))
                .filter(|p| p.exists())
        };

        if let Some(nu_dir) = &nu_config_dir {
            let nu_cache = home_path.join(".cache/tessera/init.nu");
            let _ = atomic_write_file_relaxed(&nu_cache, nu_plugin_script().as_bytes());
            let nu_cfg = nu_dir.join("config.nu");
            let snippet = format!("source {}", nu_cache.display());
            append_profile_snippet(&nu_cfg, &snippet, &mut updated);
        }

        let targets = [
            (
                home_path.join(".zshrc"),
                "eval \"$(tes env --zsh 2>/dev/null)\"",
                should_create_zshrc,
            ),
            (
                home_path.join(".bashrc"),
                "eval \"$(tes env --bash 2>/dev/null)\"",
                should_create_bashrc,
            ),
            (
                home_path.join(".bash_profile"),
                "eval \"$(tes env --bash 2>/dev/null)\"",
                home_path.join(".bash_profile").exists(),
            ),
            (
                home_path.join(".profile"),
                "eval \"$(tes env --sh 2>/dev/null)\"",
                home_path.join(".profile").exists(),
            ),
            (
                home_path.join(".kshrc"),
                "eval \"$(tes env --sh 2>/dev/null)\"",
                home_path.join(".kshrc").exists(),
            ),
            (
                home_path.join(".mkshrc"),
                "eval \"$(tes env --sh 2>/dev/null)\"",
                home_path.join(".mkshrc").exists(),
            ),
            (
                home_path.join(".tcshrc"),
                "eval \"`tes env --tcsh`\"",
                home_path.join(".tcshrc").exists(),
            ),
            (
                home_path.join(".cshrc"),
                "eval \"`tes env --tcsh`\"",
                home_path.join(".cshrc").exists(),
            ),
            (
                home_path.join(".config/fish/config.fish"),
                "command -sq tes; and tes env --fish 2>/dev/null | source",
                home_path.join(".config/fish").exists(),
            ),
            (
                home_path.join(".config/elvish/rc.elv"),
                "eval (e:tes env --elvish | slurp)",
                home_path.join(".config/elvish").exists(),
            ),
            (
                home_path.join("Documents/PowerShell/Microsoft.PowerShell_profile.ps1"),
                "Invoke-Expression (& tes env --pwsh)",
                home_path.join("Documents/PowerShell").exists(),
            ),
            (
                home_path.join("Documents/WindowsPowerShell/Microsoft.PowerShell_profile.ps1"),
                "Invoke-Expression (& tes env --pwsh)",
                home_path.join("Documents/WindowsPowerShell").exists()
                    || (cfg!(windows) && !home_path.join("Documents/PowerShell").exists()),
            ),
            (
                home_path.join(".config/powershell/Microsoft.PowerShell_profile.ps1"),
                "Invoke-Expression (& tes env --pwsh)",
                home_path.join(".config/powershell").exists(),
            ),
        ];

        for (rc_path, snippet, should_write) in targets {
            if !should_write {
                continue;
            }

            append_profile_snippet(&rc_path, snippet, &mut updated);
        }
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "configured_profiles": updated }))?
        );
    } else {
        println!(
            "Configured shell prompt integration in: {}",
            updated.join(", ")
        );
    }

    Ok(())
}

/// Atomically appends a Tessera integration snippet to `rc_path` if not already present.
fn append_profile_snippet(rc_path: &Path, snippet: &str, updated: &mut Vec<String>) {
    let existing = fs::read_to_string(rc_path).unwrap_or_default();

    if !existing.contains("tes env") && !existing.contains("tessera/init.nu") {
        let mut next = existing;

        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }

        next.push_str("\n# Tessera terminal branch prompt\n");
        next.push_str(snippet);
        next.push('\n');

        if atomic_write_file_relaxed(rc_path, next.as_bytes()).is_ok() {
            updated.push(rc_path.display().to_string());
        }
    } else {
        updated.push(format!("{} (already configured)", rc_path.display()));
    }
}

/// Executes `tes env` for shell plugin installation, plugin emission, or live environment export.
///
/// # Errors
/// Returns an error if config/OpLog reads or JSON serialization fail.
pub fn run_env(ctx: &CliContext, opts: EnvOptions) -> anyhow::Result<()> {
    if opts.install {
        return install_shell_profiles(ctx.json);
    }

    if opts.zsh {
        print!("{}", zsh_plugin_script());
        return Ok(());
    }

    if opts.bash {
        print!("{}", bash_plugin_script());
        return Ok(());
    }

    if opts.fish {
        print!("{}", fish_plugin_script());
        return Ok(());
    }

    if opts.sh {
        print!("{}", sh_plugin_script());
        return Ok(());
    }

    if opts.tcsh {
        print!("{}", tcsh_plugin_script());
        return Ok(());
    }

    if opts.nu {
        print!("{}", nu_plugin_script());
        return Ok(());
    }

    if opts.elvish {
        print!("{}", elvish_plugin_script());
        return Ok(());
    }

    if opts.pwsh {
        println!("{}", pwsh_plugin_script());
        return Ok(());
    }

    let is_fish = std::env::var("FISH_VERSION").is_ok()
        || std::env::var("TES_SHELL").is_ok_and(|s| s == "fish");
    let is_tcsh = std::env::var("TES_SHELL").is_ok_and(|s| s == "tcsh" || s == "csh");
    let is_elvish = std::env::var("TES_SHELL").is_ok_and(|s| s == "elvish");

    let Ok(coord) = Coordinator::open(&ctx.cwd) else {
        if !ctx.json {
            emit_outside_repo_cleanup(is_fish, is_tcsh, is_elvish);
        }

        return Ok(());
    };

    let session_id = detect_session_id(opts.agent.as_deref());
    let cfg = coord.oplog().load_config()?;
    let default_branch = coord
        .git()
        .current_branch_name()
        .unwrap_or_else(|_| cfg.trunk_branch.clone());
    let current = coord.oplog().load_current_reconciled()?;

    // Never call `ensure_session` during passive prompt evaluation so `OpLog` stays clean.
    let (branch_name, slot) = match current.view.agents.get(&session_id) {
        Some(a) => (
            a.target_branch
                .clone()
                .unwrap_or_else(|| default_branch.clone()),
            a.slot,
        ),
        None => (
            default_branch.clone(),
            allocate_passive_session_slot(&coord, &current.view, &session_id),
        ),
    };

    let stride = if cfg.port_stride == 0 {
        100
    } else {
        cfg.port_stride
    };
    let effective_slot = if branch_name != cfg.trunk_branch {
        coord.branch_slot(&current.view, &branch_name, &cfg.trunk_branch)
    } else {
        slot
    };
    let port = coord
        .active_leased_port_for_branch(&branch_name, &cfg.trunk_branch)
        .unwrap_or_else(|| {
            cfg.base_port
                .saturating_add(effective_slot.saturating_mul(stride))
        });
    let port_str = port.to_string();
    let port_isolation_from_cfg = std::env::var("TES_PORT_ISOLATION_CFG").as_deref() == Ok("1");
    let port_isolation = if port_isolation_from_cfg {
        cfg.port_isolation
    } else {
        cfg.is_port_isolation_enabled()
    };
    let export_cfg_marker = cfg.port_isolation;
    let shim_bin_dir = coord.sandboxes_root().join("bin");

    if port_isolation {
        workspace::ensure_polyglot_port_shims(&shim_bin_dir);
    }

    let shim_bin_str = shim_bin_dir.display().to_string();
    let port_vars = workspace::polyglot_port_env_vars(port);
    let repo_root_str = coord.git().repo_root().display().to_string();

    let ws_path_opt = if branch_name == default_branch && branch_name == cfg.trunk_branch {
        Some(repo_root_str.clone())
    } else {
        let sb_dir = coord.branch_sandbox_dir(&branch_name);
        let stamp_file = PathBuf::from(format!("{}.tree", sb_dir.display()));

        let expected_cid = coord
            .git()
            .resolve_ref(&format!("refs/heads/{branch_name}"))
            .ok()
            .flatten()
            .or_else(|| current.view.branches.get(&branch_name).copied());

        let stamp_fresh = sb_dir.is_dir()
            && expected_cid.is_some_and(|cid| {
                fs::read_to_string(&stamp_file)
                    .ok()
                    .is_some_and(|s| s.trim().starts_with(cid.as_str()))
            });

        let has_active_branch_edits = current.view.agents.values().any(|a| {
            a.target_branch.as_deref().unwrap_or(&default_branch) == branch_name
                && (!a.own_blobs.is_empty() || !a.claimed_symbols.is_empty())
        });

        if stamp_fresh && !has_active_branch_edits {
            Some(sb_dir.display().to_string())
        } else {
            if has_active_branch_edits {
                coord.reconcile_in_flight_sessions();
            }

            coord
                .prepare_branch_sandbox(&session_id, Some(&branch_name))
                .ok()
                .map(|(_, dir, _)| dir.display().to_string())
                .or_else(|| sb_dir.is_dir().then(|| sb_dir.display().to_string()))
        }
    };

    let in_preexec = std::env::var_os("_TES_IN_PREEXEC").is_some();

    let restore_dir_opt = (!in_preexec)
        .then(|| {
            coord.invocation_dir().map(|inv_dir| {
                let repo_root = coord.git().repo_root();

                if let Ok(rel) = ctx.cwd.strip_prefix(inv_dir) {
                    let candidate = repo_root.join(rel);

                    if candidate.is_dir() {
                        return candidate.display().to_string();
                    }
                }

                repo_root.display().to_string()
            })
        })
        .flatten();

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "session_id": session_id,
                "branch": branch_name,
                "slot": slot,
                "port": port,
                "port_isolation": port_isolation,
                "workspace": ws_path_opt,
                "repo_root": repo_root_str,
                "shim_bin": port_isolation.then_some(shim_bin_str),
            }))?
        );
    } else if is_fish {
        emit_fish_active_env(
            restore_dir_opt.as_deref(),
            &branch_name,
            &port_str,
            port_isolation,
            export_cfg_marker,
            &port_vars,
            &shim_bin_str,
            &repo_root_str,
            ws_path_opt.as_deref(),
        );
    } else if is_tcsh {
        emit_tcsh_active_env(
            &branch_name,
            &port_str,
            port_isolation,
            export_cfg_marker,
            &port_vars,
            &shim_bin_str,
            &repo_root_str,
            ws_path_opt.as_deref(),
        );
    } else if is_elvish {
        emit_elvish_active_env(
            &branch_name,
            &port_str,
            port_isolation,
            export_cfg_marker,
            &port_vars,
            &shim_bin_str,
            &repo_root_str,
            ws_path_opt.as_deref(),
        );
    } else {
        emit_posix_active_env(
            restore_dir_opt.as_deref(),
            &branch_name,
            &port_str,
            port_isolation,
            export_cfg_marker,
            &port_vars,
            &shim_bin_str,
            &repo_root_str,
            ws_path_opt.as_deref(),
        );
    }

    Ok(())
}

/// Emits active-repo environment commands for Fish.
#[allow(clippy::too_many_arguments)]
fn emit_fish_active_env(
    restore_dir_opt: Option<&str>,
    branch_name: &str,
    port_str: &str,
    port_isolation: bool,
    export_cfg_marker: bool,
    port_vars: &[(&'static str, String)],
    shim_bin_str: &str,
    repo_root_str: &str,
    ws_path_opt: Option<&str>,
) {
    if let Some(restore_dir) = restore_dir_opt {
        println!("builtin cd {};", fish_single_quote(restore_dir));
    }

    println!("set -gx TES_BRANCH {};", fish_single_quote(branch_name));
    println!("set -gx TES_PORT {};", fish_single_quote(port_str));

    if port_isolation {
        println!("set -gx TES_PORT_ISOLATION '1';");
        println!("set -gx TES_MANAGED_PORTS '1';");

        if export_cfg_marker {
            println!("set -gx TES_PORT_ISOLATION_CFG '1';");
        }

        for (k, v) in port_vars {
            println!("set -gx {k} {};", fish_single_quote(v));
        }

        println!("set -gx TES_SHIM_BIN {};", fish_single_quote(shim_bin_str));
        println!(
            "if test \"$PATH[1]\" != {}; set -gx PATH {} $PATH; end;",
            fish_single_quote(shim_bin_str),
            fish_single_quote(shim_bin_str)
        );
    } else {
        println!(
            "if set -q TES_SHIM_BIN; set -l _tes_idx (contains -i -- \"$TES_SHIM_BIN\" $PATH); while test -n \"$_tes_idx\"; set -e PATH[$_tes_idx]; set _tes_idx (contains -i -- \"$TES_SHIM_BIN\" $PATH); end; end;"
        );
        let non_tes = non_tes_port_keys().join(" ");
        println!(
            "if test \"$TES_MANAGED_PORTS\" = \"1\"; set -e TES_PORT_ISOLATION TES_PORT_ISOLATION_CFG TES_SHIM_BIN {non_tes} TES_MANAGED_PORTS; else; set -e TES_PORT_ISOLATION TES_PORT_ISOLATION_CFG TES_SHIM_BIN; end;"
        );
    }

    println!(
        "set -gx TES_REPO_ROOT {};",
        fish_single_quote(repo_root_str)
    );

    if let Some(ws) = ws_path_opt {
        println!("set -gx TES_WORKSPACE {};", fish_single_quote(ws));
    } else {
        println!("set -e TES_WORKSPACE;");
    }
}

/// Emits active-repo environment commands for `tcsh` / `csh`.
#[allow(clippy::too_many_arguments)]
fn emit_tcsh_active_env(
    branch_name: &str,
    port_str: &str,
    port_isolation: bool,
    export_cfg_marker: bool,
    port_vars: &[(&'static str, String)],
    shim_bin_str: &str,
    repo_root_str: &str,
    ws_path_opt: Option<&str>,
) {
    let prompt_str = format!("(tes:{branch_name}) ");
    println!("setenv TES_BRANCH {};", tcsh_quote(branch_name));
    println!("setenv TES_PORT {};", tcsh_quote(port_str));

    if port_isolation {
        println!("setenv TES_PORT_ISOLATION '1';");
        println!("setenv TES_MANAGED_PORTS '1';");

        if export_cfg_marker {
            println!("setenv TES_PORT_ISOLATION_CFG '1';");
        }

        for (k, v) in port_vars {
            println!("setenv {k} {};", tcsh_quote(v));
        }

        println!("setenv TES_SHIM_BIN {};", tcsh_quote(shim_bin_str));
    } else {
        let mut unset_cmds = String::from(
            "unsetenv TES_PORT_ISOLATION; unsetenv TES_PORT_ISOLATION_CFG; unsetenv TES_SHIM_BIN;",
        );

        if std::env::var("TES_MANAGED_PORTS").as_deref() == Ok("1") {
            for k in non_tes_port_keys() {
                let _ = write!(unset_cmds, " unsetenv {k};");
            }

            unset_cmds.push_str(" unsetenv TES_MANAGED_PORTS;");
        }

        println!("{unset_cmds}");
    }

    println!("setenv TES_REPO_ROOT {};", tcsh_quote(repo_root_str));
    println!("setenv TES_PROMPT {};", tcsh_quote(&prompt_str));

    if let Some(ws) = ws_path_opt {
        println!("setenv TES_WORKSPACE {};", tcsh_quote(ws));
    } else {
        println!("unsetenv TES_WORKSPACE;");
    }
}

/// Emits active-repo environment commands for Elvish.
#[allow(clippy::too_many_arguments)]
fn emit_elvish_active_env(
    branch_name: &str,
    port_str: &str,
    port_isolation: bool,
    export_cfg_marker: bool,
    port_vars: &[(&'static str, String)],
    shim_bin_str: &str,
    repo_root_str: &str,
    ws_path_opt: Option<&str>,
) {
    let prompt_str = format!("(tes:{branch_name}) ");
    println!("set E:TES_BRANCH = {}", elvish_single_quote(branch_name));
    println!("set E:TES_PORT = {}", elvish_single_quote(port_str));

    if port_isolation {
        println!("set E:TES_PORT_ISOLATION = '1'");
        println!("set E:TES_MANAGED_PORTS = '1'");

        if export_cfg_marker {
            println!("set E:TES_PORT_ISOLATION_CFG = '1'");
        }

        for (k, v) in port_vars {
            println!("set E:{k} = {}", elvish_single_quote(v));
        }

        println!("set E:TES_SHIM_BIN = {}", elvish_single_quote(shim_bin_str));
    } else {
        let mut managed_unsets = String::from(
            "unset-env TES_PORT_ISOLATION; unset-env TES_PORT_ISOLATION_CFG; unset-env TES_SHIM_BIN",
        );

        for k in non_tes_port_keys() {
            let _ = write!(managed_unsets, "; unset-env {k}");
        }

        managed_unsets.push_str("; unset-env TES_MANAGED_PORTS");
        println!(
            "if (and (has-env TES_MANAGED_PORTS) (eq $E:TES_MANAGED_PORTS '1')) {{ {managed_unsets} }} else {{ unset-env TES_PORT_ISOLATION; unset-env TES_PORT_ISOLATION_CFG; unset-env TES_SHIM_BIN }}"
        );
    }

    println!(
        "set E:TES_REPO_ROOT = {}",
        elvish_single_quote(repo_root_str)
    );
    println!("set E:TES_PROMPT = {}", elvish_single_quote(&prompt_str));

    if let Some(ws) = ws_path_opt {
        println!("set E:TES_WORKSPACE = {}", elvish_single_quote(ws));
    } else {
        println!("unset-env TES_WORKSPACE");
    }
}

/// Emits active-repo environment commands for POSIX shells (Zsh, Bash, sh, dash, ksh).
#[allow(clippy::too_many_arguments)]
fn emit_posix_active_env(
    restore_dir_opt: Option<&str>,
    branch_name: &str,
    port_str: &str,
    port_isolation: bool,
    export_cfg_marker: bool,
    port_vars: &[(&'static str, String)],
    shim_bin_str: &str,
    repo_root_str: &str,
    ws_path_opt: Option<&str>,
) {
    if let Some(restore_dir) = restore_dir_opt {
        println!(
            "command cd {} 2>/dev/null || true",
            posix_single_quote(restore_dir)
        );
    }

    println!("export TES_BRANCH={}", posix_single_quote(branch_name));
    println!("export TES_PORT={}", posix_single_quote(port_str));

    if port_isolation {
        println!("export TES_PORT_ISOLATION='1'");
        println!("export TES_MANAGED_PORTS='1'");

        if export_cfg_marker {
            println!("export TES_PORT_ISOLATION_CFG='1'");
        }

        for (k, v) in port_vars {
            println!("export {k}={}", posix_single_quote(v));
        }

        println!("export TES_SHIM_BIN={}", posix_single_quote(shim_bin_str));
        println!(
            r#"case "${{PATH:-}}:" in "$TES_SHIM_BIN":*) ;; *) export PATH="$TES_SHIM_BIN${{PATH:+:$PATH}}" ;; esac"#
        );
    } else {
        println!("{}", posix_strip_shim_from_path());
        let non_tes = non_tes_port_keys().join(" ");
        println!(
            "if [ \"${{TES_MANAGED_PORTS:-}}\" = \"1\" ]; then unset TES_PORT_ISOLATION TES_SHIM_BIN {non_tes} TES_MANAGED_PORTS TES_PORT_ISOLATION_CFG; else unset TES_PORT_ISOLATION TES_PORT_ISOLATION_CFG TES_SHIM_BIN; fi"
        );
    }

    println!("export TES_REPO_ROOT={}", posix_single_quote(repo_root_str));

    if let Some(ws) = ws_path_opt {
        println!("export TES_WORKSPACE={}", posix_single_quote(ws));
    } else {
        println!("unset TES_WORKSPACE");
    }
}

/// Emits cleanup commands when invoked outside a Tessera repository, removing `TES_SHIM_BIN`
/// from `PATH` and unsetting framework port variables ONLY if Tessera itself exported them.
fn emit_outside_repo_cleanup(is_fish: bool, is_tcsh: bool, is_elvish: bool) {
    let non_tes = non_tes_port_keys().join(" ");

    if is_fish {
        println!(
            "if set -q TES_SHIM_BIN; set -l _tes_idx (contains -i -- \"$TES_SHIM_BIN\" $PATH); while test -n \"$_tes_idx\"; set -e PATH[$_tes_idx]; set _tes_idx (contains -i -- \"$TES_SHIM_BIN\" $PATH); end; end;"
        );
        println!(
            "if test \"$TES_MANAGED_PORTS\" = \"1\"; set -e TES_BRANCH TES_PORT TES_WORKSPACE TES_REPO_ROOT TES_PORT_ISOLATION TES_PORT_ISOLATION_CFG TES_SHIM_BIN _TES_PREV_PWD {non_tes} TES_MANAGED_PORTS; else; set -e TES_BRANCH TES_PORT TES_WORKSPACE TES_REPO_ROOT TES_PORT_ISOLATION TES_PORT_ISOLATION_CFG TES_SHIM_BIN _TES_PREV_PWD; end;"
        );
    } else if is_tcsh {
        let mut unset_cmds = String::from(
            "unsetenv TES_BRANCH; unsetenv TES_PORT; unsetenv TES_WORKSPACE; unsetenv TES_REPO_ROOT; unsetenv TES_PORT_ISOLATION; unsetenv TES_PORT_ISOLATION_CFG; unsetenv TES_SHIM_BIN; unsetenv TES_PROMPT; unsetenv _TES_PREV_PWD;",
        );

        if std::env::var("TES_MANAGED_PORTS").as_deref() == Ok("1") {
            for k in non_tes_port_keys() {
                let _ = write!(unset_cmds, " unsetenv {k};");
            }

            unset_cmds.push_str(" unsetenv TES_MANAGED_PORTS;");
        }

        println!("{unset_cmds}");
    } else if is_elvish {
        let base_unsets = "unset-env TES_BRANCH; unset-env TES_PORT; unset-env TES_WORKSPACE; unset-env TES_REPO_ROOT; unset-env TES_PORT_ISOLATION; unset-env TES_PORT_ISOLATION_CFG; unset-env TES_SHIM_BIN; unset-env TES_PROMPT";
        let mut managed_unsets = String::from(base_unsets);

        for k in non_tes_port_keys() {
            let _ = write!(managed_unsets, "; unset-env {k}");
        }

        managed_unsets.push_str("; unset-env TES_MANAGED_PORTS");
        println!(
            "if (and (has-env TES_MANAGED_PORTS) (eq $E:TES_MANAGED_PORTS '1')) {{ {managed_unsets} }} else {{ {base_unsets} }}"
        );
    } else {
        println!("{}", posix_strip_shim_from_path());
        println!(
            "if [ \"${{TES_MANAGED_PORTS:-}}\" = \"1\" ]; then unset TES_BRANCH TES_PORT TES_WORKSPACE TES_REPO_ROOT TES_PORT_ISOLATION TES_SHIM_BIN _TES_PREV_PWD {non_tes} TES_MANAGED_PORTS TES_PORT_ISOLATION_CFG; else unset TES_BRANCH TES_PORT TES_WORKSPACE TES_REPO_ROOT TES_PORT_ISOLATION TES_PORT_ISOLATION_CFG TES_SHIM_BIN _TES_PREV_PWD; fi"
        );
    }
}
