# Same-Branch Editing

This example walks through two agents editing the same function in `src/currency.ts` on `main`.

<SameBranchAstIllustration />

## Starting File

Suppose `src/currency.ts` on `main` contains:

```ts
export function formatCurrency(cents: number, currency: string = "USD"): string {
  const amount = (cents / 100).toFixed(2);
  return `${currency} ${amount}`;
}
```

## Prompting Two Agents

Open two AI agent tabs and give both a prompt that modifies `formatCurrency`:

- **Tab 1**: *"Add a `locale: string = 'en-US'` parameter to `formatCurrency` in `src/currency.ts`."*
- **Tab 2**: *"Add a `decimals: number = 2` parameter to `formatCurrency` in `src/currency.ts`."*

## How It Works

1. Both agents claim `src/currency.ts` against the same starting commit on `main`.
2. Agent 1 writes its edit and runs `tes hook post-tool --path src/currency.ts`.
3. Agent 2 writes its edit and runs `tes hook post-tool --path src/currency.ts`.
4. Tessera parses the base version and both agents' versions using Tree-sitter and merges the parameter list and function body automatically.

## Merged Result

```ts
export function formatCurrency(
  cents: number,
  currency: string = "USD",
  locale: string = "en-US",
  decimals: number = 2
): string {
  const amount = (cents / 100).toFixed(decimals);
  return `[${locale}] ${currency} ${amount}`;
}
```

## Committing

Check the status and commit:

```bash:no-line-numbers
tes status
tes diff
tes commit -m "feat(currency): support locale and decimals parameters"
```

## Next Steps

- [Feature Branches](/examples/parallel-branches) - Isolate two features on separate branches
- [Conflict Resolution](/examples/conflict-resolution) - Handle overlapping edits to the same expression
