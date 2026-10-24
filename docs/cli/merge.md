# tes merge

Commit the current session or semantically merge multiple branches into a target branch.

## Usage

```bash:no-line-numbers
tes merge [branches_or_sessions...] [options]
```

## Options

- `--into <BRANCH>` - Target branch to merge into (defaults to the active session's branch or `main`).
- `-m, --message <MSG>` - Commit message for the merge commit.
- `--verify <CMD>` - Verification command to run before finalizing the merge.
- `--keep-open` - Keep agent sessions open after merging.
- `--co-author [=<NAME <EMAIL>>]` - Append a `Co-authored-by:` trailer to the merge commit (defaults to configured `co_author_trailer` when passed without a value; repeatable).
- `--no-co-author` - Omit `Co-authored-by:` trailers even if `co_author = true` in repository config.
- `--author <NAME <EMAIL>>` - Override the commit `Author` identity for the merge commit.

## Description

When called with `-m "<message>"` inside an active session, `tes merge` commits the session's changes onto its target branch.

When called with one or more branch names and `--into <target>`, `tes merge` combines the branches into `<target>` using Tessera's semantic merge pipeline (subtree skipping, rename detection, SQL migration resequencing, manifest drivers, and 3-way Tree-sitter AST merging).

Merge commits record your active Git/system user as the commit `Author` (or `--author "Name <email>"` when specified), and optionally append `Co-authored-by: Tessera <noreply@tessera.getlunaris.dev>` when `co_author = true` is configured (`false` by default) or `--co-author` is passed.

## Examples

```bash:no-line-numbers
# commit the current session onto its target branch
tes merge -m "feat(pricing): add pricing calculator"

# merge two feature branches into main
tes merge feat/pricing feat/faq --into main -m "feat: add pricing and faq"

# merge with an explicit Co-authored-by trailer
tes merge feat/pricing --into main -m "feat: add pricing" --co-author

# run a verification command before completing the merge
tes merge feat/pricing --into main --verify "npm run build" -m "feat: add pricing"
```
