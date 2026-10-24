# tes commit

Commit the current session's changes onto its target branch.

## Usage

```bash:no-line-numbers
tes commit [-m <MESSAGE>] [options]
```

## Options

- `-m, --message <MSG>` - Commit message.
- `-b, --branch <BRANCH>` - Explicit target branch override.
- `--amend` - Amend the tip commit of the target branch instead of creating a new child commit.
- `--no-edit` - Reuse the previous commit message when amending without `-m`.
- `--verify <CMD>` - Run a verification command before committing.
- `--co-author [=<NAME <EMAIL>>]` - Append a `Co-authored-by:` trailer to this commit (defaults to configured `co_author_trailer` when passed without a value; repeatable).
- `--no-co-author` - Omit `Co-authored-by:` trailers for this commit even if `co_author = true` in repository config.
- `--author <NAME <EMAIL>>` - Override the commit `Author` identity for this commit.
- `-a, --agent <ID>` - Explicit session ID.

## Description

`tes commit` snapshots any pending changes in the current session, writes a new commit onto the session's target branch (`refs/heads/<branch>`), and updates the branch sandbox. Passing `--amend` replaces the tip commit on the target branch while preserving its parent commits (and reusing the previous commit message and author when `-m` / `--author` are omitted).

Commits record your active Git/system user (`git config user.name` and `user.email`, with automatic OS user fallback) as the commit `Author`, unless overridden via `--author "Name <email>"`. If `co_author = true` is enabled in `tessera.toml`, via `tes config set co_author true` (`false` by default), or per-commit via `--co-author`, Tessera appends `Co-authored-by: Tessera <noreply@tessera.getlunaris.dev>` (or your custom trailer) to the commit message.

## Examples

```bash:no-line-numbers
# commit changes in the current session
tes commit -m "feat(pricing): add interactive pricing calculator"

# amend the previous commit with new edits or an updated message
tes commit --amend --no-edit
tes commit --amend -m "feat(pricing): add interactive pricing calculator and unit tests"

# commit to a specific branch
tes commit -b feat/pricing -m "feat(pricing): add interactive pricing calculator"

# include the default Co-authored-by trailer on a single commit
tes commit -m "feat(pricing): add tier selector" --co-author

# include a custom Co-authored-by trailer or override the commit author
tes commit -m "feat(pricing): pair on billing logic" --co-author "Alice <alice@example.com>"
tes commit -m "fix(auth): token refresh" --author "Jane Doe <jane@example.com>"
```
