# tes exec

Run a command inside an isolated Copy-on-Write branch sandbox.

## Usage

```bash:no-line-numbers
tes exec [-b <branch>] -- <command...>
```

## Options

- `-b, --branch <BRANCH>` - Target branch sandbox to run the command in (defaults to the current session's branch).
- `-a, --agent <ID>` - Explicit session ID.

## Description

`tes exec` materializes the target branch's files into a Copy-on-Write sandbox, clones dependency and build caches (`node_modules`, `target`, `.venv`, `vendor`), prepends `<sandbox>/node_modules/.bin` to `PATH`, and exports isolated `PORT`, `TES_PORT`, `TES_BRANCH`, and `DATABASE_URL` environment variables.

## Examples

```bash:no-line-numbers
# run a build on the current session's branch
tes exec -- npm run build

# run a build on a specific branch without switching your working tree
tes exec -b feat/pricing -- npm run build

# run tests on two branches in parallel
tes exec -b feat/pricing -- npm test
tes exec -b feat/faq -- npm test
```
