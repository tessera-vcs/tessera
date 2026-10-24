# Examples

Step-by-step walkthroughs for working with multiple agents and branches in Tessera.

## Available Examples

- [Same-Branch Editing](/examples/same-branch) - Two agents editing the same function or file on `main`
- [Feature Branches](/examples/parallel-branches) - Building two features on separate branches and merging them into `main`
- [Conflict Resolution](/examples/conflict-resolution) - Handling incompatible edits on the same line using the editor or CLI

## Quick Examples

### Initialize and Configure Shell

```bash:no-line-numbers
tes init
tes env --install
```

### Switch Terminals to Different Branches

```bash:no-line-numbers
# Terminal 1
tes switch feat/pricing
npm run dev

# Terminal 2
tes switch feat/faq
npm run dev
```

### Merge Multiple Feature Branches into `main`

```bash:no-line-numbers
tes merge feat/pricing feat/faq --into main -m "feat: add pricing and faq"
```
