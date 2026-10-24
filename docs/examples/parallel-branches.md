# Feature Branches

This example shows two agents building separate features on `feat/pricing` and `feat/faq` while both modifying `src/App.vue` in the same checkout, then merging both branches into `main`.

<HowTesseraWorks />

## Starting File

Suppose `src/App.vue` on `main` starts as:

```vue
<script setup lang="ts">
import HelloWorld from './components/HelloWorld.vue'
</script>

<template>
  <HelloWorld />
</template>
```

## 1. Launch Two Agents on Separate Branches

Open two AI agent chat tabs in your editor:

- **Tab 1 (`feat/pricing`)**:
  > *"On branch `feat/pricing`, create `src/components/PricingCalculator.vue`, add it to `src/App.vue`, and verify with `npm run build`."*

- **Tab 2 (`feat/faq`)**:
  > *"On branch `feat/faq`, create `src/components/TestimonialsFaq.vue`, add it to `src/App.vue`, and verify with `npm run build`."*

## 2. Branch Isolation

Even though both agents edit `src/App.vue` in the same folder at the same time:

- `feat/pricing` only contains `HelloWorld.vue` and `PricingCalculator.vue`.
- `feat/faq` only contains `HelloWorld.vue` and `TestimonialsFaq.vue`.
- When each agent runs `tes exec -- npm run build`, the build runs inside that branch's Copy-on-Write sandbox (`branches/feat-pricing` and `branches/feat-faq`), so neither branch fails with missing-component errors.

## 3. Preview Either Branch

You can inspect the diff of each branch or switch your editor to preview it:

```bash:no-line-numbers
# Compare each branch against main
tes diff -b feat/pricing
tes diff -b feat/faq

# Or switch your editor working tree to feat/pricing
tes switch feat/pricing --editor
```

## 4. Merge Both Branches into `main`

Combine both branches into `main` with a single command:

```bash:no-line-numbers
tes merge feat/pricing feat/faq --into main -m "feat: add pricing calculator and testimonials faq"
```

Tessera merges both `<script setup>` import blocks and both `<template>` usages cleanly into `main`:

```vue
<script setup lang="ts">
import HelloWorld from './components/HelloWorld.vue'
import PricingCalculator from './components/PricingCalculator.vue'
import TestimonialsFaq from './components/TestimonialsFaq.vue'
</script>

<template>
  <HelloWorld />
  <PricingCalculator />
  <TestimonialsFaq />
</template>
```

## Next Steps

- [Conflict Resolution](/examples/conflict-resolution) - See what happens when two branches change the exact same line
- [Branches](/guides/branches-and-terminals) - Learn more about branch management
