# Tessera for VS Code & Antigravity IDE

Visual branch management, multi-agent line attribution, 2D commit graph, and 3-way AST conflict resolution for repositories initialized with [Tessera (`tes`)](https://tessera.getlunaris.dev).

## Features

- **Status Bar Switcher**: Shows the active branch in your editor working tree. Click at any time to switch branches or open the Branch Graph.
- **Live Multi-Agent Attribution**: Highlights line ranges modified concurrently by AI coding agents and terminal sessions on the active branch, complete with gutter markers, hover details, and **Jump to Session** CodeLenses.
- **2D Branch & Commit Graph**: Interactive DAG canvas displaying parallel branch lanes, sandbox ports, active agent sessions, and one-click branch switching and merging.
- **3-Way AST Conflict Resolver**: Side-by-side comparison of competing agent intents (`Ours`, `Theirs`, and `AST-Union` preview) with one-click resolution.
- **Sidebar Tree Views**: Dedicated views for **Branches**, **Sessions**, and **Conflicts & Checkpoints**.

## Getting Started

1. Install the `tes` CLI:
   ```bash
   curl -fsSL https://tessera.getlunaris.dev/install.sh | sh
   ```
2. Initialize your Git repository:
   ```bash
   tes init
   ```

## License

Released under the [MIT License](./LICENSE).
