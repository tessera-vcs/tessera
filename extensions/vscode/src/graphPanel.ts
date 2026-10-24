/**
 * Interactive 2D spatial branch and commit DAG webview panel (`tes.openGraph`).
 */

import * as crypto from "crypto";
import * as vscode from "vscode";
import {
  AgentEntry,
  BranchDetail,
  CliClient,
  CommitGraphNode,
  ConflictItem,
  WorkspaceStateSnapshot,
} from "./client";

/**
 * Interactive 2D Spatial Branch & Commit DAG Webview Panel (`tes.openGraph`).
 */
export class VisualGraphPanel {
  private static currentPanel: VisualGraphPanel | undefined;
  private readonly panel: vscode.WebviewPanel;
  private readonly client: CliClient;
  private readonly onSwitchBranch: (branch: string) => Promise<void>;
  private readonly onRefresh: () => Promise<void>;
  private readonly disposables: vscode.Disposable[] = [];
  private readonly expandedGroups = new Set<string>();
  private readonly expandedHorizGroups = new Set<string>();
  private expandAll = false;
  private lastSnapshot: WorkspaceStateSnapshot;

  /**
   * Initializes the graph webview panel and registers interactive message handlers.
   *
   * @param panel The underlying VS Code webview panel.
   * @param client The CLI client for running repository commands.
   * @param snapshot The initial repository state snapshot.
   * @param onSwitchBranch Callback invoked when the user switches branches from the graph.
   * @param onRefresh Callback invoked when the graph requests a state refresh.
   */
  private constructor(
    panel: vscode.WebviewPanel,
    client: CliClient,
    snapshot: WorkspaceStateSnapshot,
    onSwitchBranch: (branch: string) => Promise<void>,
    onRefresh: () => Promise<void>
  ) {
    this.panel = panel;
    this.client = client;
    this.lastSnapshot = snapshot;
    this.onSwitchBranch = onSwitchBranch;
    this.onRefresh = onRefresh;

    this.panel.webview.html = this.buildHtml(snapshot);

    this.panel.webview.onDidReceiveMessage(
      async (msg: {
        readonly command: string;
        readonly branch?: string;
        readonly sources?: readonly string[];
        readonly target?: string;
        readonly filePath?: string;
        readonly strategy?: "ours" | "theirs" | "ast-union" | "working";
        readonly groupId?: string;
      }) => {
        try {
          if (msg.command === "switchBranch" && msg.branch) {
            await this.onSwitchBranch(msg.branch);
          } else if (msg.command === "expandGroup" && msg.groupId) {
            this.expandedGroups.add(msg.groupId);
            this.panel.webview.html = this.buildHtml(this.lastSnapshot);
          } else if (msg.command === "toggleHorizGroup" && msg.groupId) {
            if (this.expandedHorizGroups.has(msg.groupId)) {
              this.expandedHorizGroups.delete(msg.groupId);
            } else {
              this.expandedHorizGroups.add(msg.groupId);
            }

            this.panel.webview.html = this.buildHtml(this.lastSnapshot);
          } else if (msg.command === "collapseAll") {
            this.expandAll = false;
            this.expandedGroups.clear();
            this.expandedHorizGroups.clear();
            this.panel.webview.html = this.buildHtml(this.lastSnapshot);
          } else if (msg.command === "expandAll") {
            this.expandAll = true;
            this.panel.webview.html = this.buildHtml(this.lastSnapshot);
          } else if (msg.command === "mergeInto" && msg.sources && msg.target) {
            const defaultMsg = `Merge ${msg.sources.join(", ")} into ${msg.target}`;
            const commitMsg = await vscode.window.showInputBox({
              prompt: `Merge ${msg.sources.join(", ")} into ${msg.target}`,
              value: defaultMsg,
            });

            if (!commitMsg) {
              return;
            }

            await this.client.mergeInto(msg.sources, msg.target, commitMsg);
            await this.onRefresh();
          } else if (msg.command === "openBranchDiff" && msg.branch && msg.filePath) {
            const cleanPath = msg.filePath.split("::")[0]?.trim() || msg.filePath;
            await vscode.commands.executeCommand("tes.diffBranchFile", msg.branch, cleanPath);
          } else if (msg.command === "openFile" && msg.filePath) {
            const cleanPath = msg.filePath.split("::")[0]?.trim() || msg.filePath;
            const fullUri = vscode.Uri.file(`${this.client.getRepoRoot()}/${cleanPath}`);
            const doc = await vscode.workspace.openTextDocument(fullUri);
            const editor = await vscode.window.showTextDocument(doc, { preview: false });
            const text = doc.getText();
            const markerIdx = text.indexOf("<<<<<<<");

            if (markerIdx >= 0) {
              const pos = doc.positionAt(markerIdx);
              editor.selection = new vscode.Selection(pos, pos);
              editor.revealRange(
                new vscode.Range(pos, pos),
                vscode.TextEditorRevealType.InCenter
              );
            }
          } else if (msg.command === "resolveConflict" && msg.filePath && msg.strategy) {
            await this.client.resolveConflict(msg.filePath, msg.strategy);
            await this.onRefresh();
          } else if (msg.command === "deleteBranch" && msg.branch) {
            await vscode.commands.executeCommand("tes.deleteBranch", msg.branch);
          } else if (msg.command === "discardWorking" && msg.branch) {
            await vscode.commands.executeCommand("tes.discardWorkingChanges", msg.branch);
          } else if (msg.command === "refresh") {
            await this.onRefresh();
          }
        } catch (err: unknown) {
          const message = err instanceof Error ? err.message : String(err);
          void vscode.window.showErrorMessage(`Branch Graph: ${message}`);
        }
      },
      null,
      this.disposables
    );

    this.panel.onDidDispose(() => this.dispose(), null, this.disposables);
  }

  /**
   * Opens or reveals the 2D Branch & Commit Graph webview panel.
   *
   * @param client The CLI client for running repository commands.
   * @param snapshot The current repository state snapshot.
   * @param onSwitchBranch Callback invoked when switching branches.
   * @param onRefresh Callback invoked when refreshing state.
   */
  public static show(
    client: CliClient,
    snapshot: WorkspaceStateSnapshot,
    onSwitchBranch: (branch: string) => Promise<void>,
    onRefresh: () => Promise<void>
  ): void {
    if (VisualGraphPanel.currentPanel) {
      VisualGraphPanel.currentPanel.panel.reveal(vscode.ViewColumn.One);
      VisualGraphPanel.currentPanel.update(snapshot);
      return;
    }

    const panel = vscode.window.createWebviewPanel(
      "visualGraph",
      "Branch Graph",
      vscode.ViewColumn.One,
      {
        enableScripts: true,
        retainContextWhenHidden: true,
      }
    );

    VisualGraphPanel.currentPanel = new VisualGraphPanel(
      panel,
      client,
      snapshot,
      onSwitchBranch,
      onRefresh
    );
  }

  /**
   * Updates the open graph webview if it is currently visible.
   *
   * @param snapshot The updated repository state snapshot.
   */
  public static updateIfVisible(snapshot: WorkspaceStateSnapshot): void {
    if (VisualGraphPanel.currentPanel) {
      VisualGraphPanel.currentPanel.update(snapshot);
    }
  }

  /**
   * Re-renders the graph webview with a new repository snapshot.
   *
   * @param snapshot The updated repository state snapshot.
   */
  public update(snapshot: WorkspaceStateSnapshot): void {
    this.lastSnapshot = snapshot;
    this.panel.webview.html = this.buildHtml(snapshot);
  }

  /**
   * Disposes the webview panel and all registered event subscriptions.
   */
  private dispose(): void {
    VisualGraphPanel.currentPanel = undefined;
    this.panel.dispose();

    while (this.disposables.length > 0) {
      const d = this.disposables.pop();

      if (d) {
        d.dispose();
      }
    }
  }

  /**
   * Computes the 2D topological DAG layout and generates the HTML/SVG canvas document.
   *
   * @param snapshot The repository state snapshot to render.
   * @returns Complete HTML markup for the graph webview.
   */
  private buildHtml(snapshot: WorkspaceStateSnapshot): string {
    const { branchReport, radarReport, conflicts } = snapshot;
    const editorBranch = branchReport.editor_branch || branchReport.active_branch || "main";
    const details: readonly BranchDetail[] = branchReport.branch_details;
    const rawCommits: readonly CommitGraphNode[] = branchReport.commit_graph;
    const agents: readonly AgentEntry[] = radarReport.active_agents;

    const trunkBranch = details.some((b) => b.name === "main")
      ? "main"
      : details[0]?.name ?? "main";

    const sortedBranches = [...details].sort((a, b) => {
      if (a.name === trunkBranch) {
        return -1;
      }

      if (b.name === trunkBranch) {
        return 1;
      }

      return a.name.localeCompare(b.name);
    });

    const branchColMap = new Map<string, number>();
    sortedBranches.forEach((b, idx) => branchColMap.set(b.name, idx));
    const numCols = Math.max(sortedBranches.length, 1);

    interface MutableCommit {
      readonly sha: string;
      readonly short_sha: string;
      readonly subject: string;
      readonly author: string;
      readonly timestamp: number;
      parents: string[];
      readonly refs: string[];
      readonly collapsedCount?: number;
      readonly groupId?: string;
      readonly horizMembers?: readonly MutableCommit[];
      readonly horizTopic?: string;
    }

    const allCommits: MutableCommit[] = rawCommits.map((c) => ({
      ...c,
      parents: [...c.parents],
      refs: [...c.refs],
    }));

    const branchTipFullSha = new Map<string, string>();
    for (const b of sortedBranches) {
      const match = allCommits.find(
        (c) => c.sha === b.commit || c.sha.startsWith(b.short_commit)
      );
      if (match) {
        branchTipFullSha.set(b.name, match.sha);
      }
    }

    // Backfill multi-parent merge links for commits whose subject is `Merge <b1> and <b2> into <target>`
    // in case they were created by an earlier single-parent commit call.
    for (const c of allCommits) {
      if (c.parents.length <= 1 && /^Merge\s+.+\s+into\s+/i.test(c.subject)) {
        for (const b of sortedBranches) {
          if (b.name !== trunkBranch && c.subject.includes(b.name)) {
            const tipSha = branchTipFullSha.get(b.name);
            if (tipSha && tipSha !== c.sha && !c.parents.includes(tipSha)) {
              c.parents.push(tipSha);
            }
          }
        }
      }
    }

    const rawCommitMap = new Map<string, MutableCommit>();
    for (const c of allCommits) {
      rawCommitMap.set(c.sha, c);
    }

    // Keep the trunk first-parent chain in column 0 (`trunkBranch`), and place
    // non-trunk branch commits into their dedicated branch columns.
    const commitCol = new Map<string, number>();
    const trunkTip = branchTipFullSha.get(trunkBranch) ?? allCommits[0]?.sha;
    if (trunkTip) {
      let cur: string | undefined = trunkTip;
      const visited = new Set<string>();
      while (cur && !visited.has(cur)) {
        visited.add(cur);
        commitCol.set(cur, 0);
        const node = rawCommitMap.get(cur);
        cur = node?.parents[0];
      }
    }

    for (const b of sortedBranches) {
      if (b.name === trunkBranch) {
        continue;
      }

      const col = branchColMap.get(b.name) ?? 0;
      let cur = branchTipFullSha.get(b.name);
      const visited = new Set<string>();
      while (cur && !visited.has(cur) && !commitCol.has(cur)) {
        visited.add(cur);
        commitCol.set(cur, col);
        const node = rawCommitMap.get(cur);
        cur = node?.parents[0];
      }
    }

    for (const c of allCommits) {
      if (!commitCol.has(c.sha)) {
        commitCol.set(c.sha, 0);
      }
    }

    const rawChildrenMap = new Map<string, string[]>();
    for (const c of allCommits) {
      for (const p of c.parents) {
        if (rawCommitMap.has(p)) {
          const list = rawChildrenMap.get(p) ?? [];
          list.push(c.sha);
          rawChildrenMap.set(p, list);
        }
      }
    }

    const branchTipShasSet = new Set<string>(branchTipFullSha.values());

    // Group consecutive single-parent commits with the exact same conventional topic/scope
    // into a spine anchor card that can expand horizontally to the right when clicked.
    const hasCrossColRawChild = (sha: string): boolean => {
      const col = commitCol.get(sha) ?? 0;
      const ch = rawChildrenMap.get(sha) ?? [];
      return ch.some((childSha) => (commitCol.get(childSha) ?? 0) !== col);
    };

    const horizParentOf = new Map<string, string>();
    const horizChildOf = new Map<string, string>();
    for (const c of allCommits) {
      if (
        c.parents.length !== 1 ||
        branchTipShasSet.has(c.sha) ||
        c.refs.some((r) => r.includes("HEAD")) ||
        hasCrossColRawChild(c.sha)
      ) {
        continue;
      }

      const parentSha = c.parents[0];
      if (!parentSha) {
        continue;
      }

      const parentNode = rawCommitMap.get(parentSha);
      if (
        !parentNode ||
        parentNode.parents.length > 1 ||
        branchTipShasSet.has(parentSha) ||
        parentNode.refs.some((r) => r.includes("HEAD")) ||
        (commitCol.get(parentSha) ?? 0) !== (commitCol.get(c.sha) ?? 0) ||
        (rawChildrenMap.get(parentSha)?.length ?? 0) !== 1 ||
        hasCrossColRawChild(parentSha)
      ) {
        continue;
      }

      const childTopic = this.extractCommitTopic(c.subject);
      const parentTopic = this.extractCommitTopic(parentNode.subject);
      if (this.areCommitTopicsRelated(childTopic, parentTopic)) {
        horizParentOf.set(c.sha, parentSha);
        horizChildOf.set(parentSha, c.sha);
      }
    }

    const replacedByHorizAnchor = new Map<string, string>();
    const horizAnchorBySha = new Map<string, MutableCommit>();
    const horizFoldedShas = new Set<string>();

    for (const c of allCommits) {
      if (horizChildOf.has(c.sha) || !horizParentOf.has(c.sha)) {
        continue;
      }

      // `c` is the newest commit of a consecutive related sequence; walk down to the oldest commit in the sequence.
      const seq: MutableCommit[] = [c];
      let cur = horizParentOf.get(c.sha);
      while (cur) {
        const node = rawCommitMap.get(cur);
        if (!node) {
          break;
        }

        seq.push(node);
        cur = horizParentOf.get(cur);
      }

      if (seq.length < 2) {
        continue;
      }

      const oldest = seq[seq.length - 1] ?? c;
      const topic = this.extractCommitTopic(c.subject);
      horizAnchorBySha.set(c.sha, {
        ...c,
        parents: [...oldest.parents],
        horizMembers: seq.slice(1),
        horizTopic: topic,
      });

      for (const member of seq.slice(1)) {
        replacedByHorizAnchor.set(member.sha, c.sha);
        horizFoldedShas.add(member.sha);
      }
    }

    const spineCommits: MutableCommit[] = [];
    for (const c of allCommits) {
      const anchor = horizAnchorBySha.get(c.sha);
      if (anchor) {
        spineCommits.push(anchor);
      } else if (!horizFoldedShas.has(c.sha)) {
        spineCommits.push({
          ...c,
          parents: c.parents.map((p) => replacedByHorizAnchor.get(p) ?? p),
        });
      }
    }

    for (const c of spineCommits) {
      c.parents = c.parents.map((p) => replacedByHorizAnchor.get(p) ?? p);
    }

    const spineCommitMap = new Map<string, MutableCommit>();
    for (const c of spineCommits) {
      spineCommitMap.set(c.sha, c);
    }

    // Identify older linear spine chains that can be folded into compact `+N commits` vertical pills.
    const childrenMap = new Map<string, string[]>();
    for (const c of spineCommits) {
      for (const p of c.parents) {
        if (spineCommitMap.has(p)) {
          const list = childrenMap.get(p) ?? [];
          list.push(c.sha);
          childrenMap.set(p, list);
        }
      }
    }

    const pinnedShas = new Set<string>(branchTipShasSet);

    for (const b of sortedBranches) {
      const keepRecent = b.name === trunkBranch ? 6 : 3;
      const rawTip = branchTipFullSha.get(b.name);
      let cur = rawTip ? (replacedByHorizAnchor.get(rawTip) ?? rawTip) : undefined;
      for (let i = 0; i < keepRecent && cur; i++) {
        pinnedShas.add(cur);
        cur = spineCommitMap.get(cur)?.parents[0];
      }
    }

    for (const c of spineCommits) {
      const col = commitCol.get(c.sha) ?? 0;
      const children = childrenMap.get(c.sha) ?? [];
      const hasCrossColChild = children.some((ch) => (commitCol.get(ch) ?? 0) !== col);
      const isHeadRef = c.refs.some((r) => r.includes("HEAD"));
      if (c.parents.length > 1 || children.length > 1 || hasCrossColChild || isHeadRef) {
        pinnedShas.add(c.sha);
      }
    }

    const isFoldable = (sha: string): boolean => {
      if (pinnedShas.has(sha)) {
        return false;
      }

      const c = spineCommitMap.get(sha);
      if (!c || c.parents.length > 1) {
        return false;
      }

      return true;
    };

    const foldableParentOf = new Map<string, string>();
    const foldableChildOf = new Map<string, string>();
    for (const c of spineCommits) {
      if (!isFoldable(c.sha)) {
        continue;
      }

      const parentSha = c.parents[0];
      if (
        parentSha &&
        spineCommitMap.has(parentSha) &&
        isFoldable(parentSha) &&
        (commitCol.get(parentSha) ?? 0) === (commitCol.get(c.sha) ?? 0) &&
        (childrenMap.get(parentSha)?.length ?? 0) === 1
      ) {
        foldableParentOf.set(c.sha, parentSha);
        foldableChildOf.set(parentSha, c.sha);
      }
    }

    const replacedByCollapsedId = new Map<string, string>();
    const collapsedNodesByNewest = new Map<string, MutableCommit>();
    const foldedMemberShas = new Set<string>();
    let totalCollapsibleGroups = 0;

    for (const c of spineCommits) {
      if (!isFoldable(c.sha) || foldableChildOf.has(c.sha)) {
        continue;
      }

      // `c` is the newest spine commit of a maximal foldable chain; walk down to its oldest foldable ancestor.
      const chain: MutableCommit[] = [c];
      let cur = foldableParentOf.get(c.sha);
      while (cur) {
        const node = spineCommitMap.get(cur);
        if (!node) {
          break;
        }

        chain.push(node);
        cur = foldableParentOf.get(cur);
      }

      if (chain.length < 2) {
        continue;
      }

      totalCollapsibleGroups++;
      const groupId = c.sha;
      if (this.expandAll || this.expandedGroups.has(groupId)) {
        continue;
      }

      const oldest = chain[chain.length - 1] ?? c;
      const oldestRaw =
        oldest.horizMembers && oldest.horizMembers.length > 0
          ? (oldest.horizMembers[oldest.horizMembers.length - 1] ?? oldest)
          : oldest;
      const totalRawCount = chain.reduce(
        (sum, item) => sum + 1 + (item.horizMembers?.length ?? 0),
        0
      );
      const collapsedId = `collapsed:${groupId}`;
      const col = commitCol.get(c.sha) ?? 0;
      commitCol.set(collapsedId, col);

      collapsedNodesByNewest.set(c.sha, {
        sha: collapsedId,
        short_sha: `+${totalRawCount} commits`,
        subject: `${oldestRaw.short_sha} .. ${c.short_sha}`,
        author: c.author,
        timestamp: c.timestamp,
        parents: [...oldest.parents],
        refs: [],
        collapsedCount: totalRawCount,
        groupId,
      });

      replacedByCollapsedId.set(c.sha, collapsedId);
      for (const item of chain) {
        foldedMemberShas.add(item.sha);
      }
    }

    const commits: MutableCommit[] = [];
    for (const c of spineCommits) {
      const replacement = collapsedNodesByNewest.get(c.sha);
      if (replacement) {
        commits.push(replacement);
      } else if (!foldedMemberShas.has(c.sha)) {
        commits.push({
          ...c,
          parents: c.parents.map((p) => replacedByCollapsedId.get(p) ?? p),
        });
      }
    }

    for (const c of commits) {
      c.parents = c.parents.map((p) => replacedByCollapsedId.get(p) ?? p);
    }

    const commitMap = new Map<string, MutableCommit>();
    for (const c of commits) {
      commitMap.set(c.sha, c);
    }

    // Compute bottom-up topological level so parallel branch commits sit on the same rank.
    const memoLevel = new Map<string, number>();
    const visiting = new Set<string>();
    const computeLevel = (sha: string): number => {
      const cached = memoLevel.get(sha);
      if (cached !== undefined) {
        return cached;
      }

      if (visiting.has(sha)) {
        return 0;
      }

      visiting.add(sha);
      const node = commitMap.get(sha);
      if (!node || node.parents.length === 0) {
        memoLevel.set(sha, 0);
        visiting.delete(sha);
        return 0;
      }

      let maxParentLevel = -1;
      for (const p of node.parents) {
        if (commitMap.has(p)) {
          maxParentLevel = Math.max(maxParentLevel, computeLevel(p));
        }
      }

      const lvl = maxParentLevel + 1;
      memoLevel.set(sha, lvl);
      visiting.delete(sha);
      return lvl;
    };

    for (const c of commits) {
      computeLevel(c.sha);
    }

    const occupiedCells = new Set<string>();
    const commitsBottomUp = [...commits].reverse();
    for (const c of commitsBottomUp) {
      const col = commitCol.get(c.sha) ?? 0;
      let lvl = memoLevel.get(c.sha) ?? 0;
      for (const p of c.parents) {
        const pLvl = memoLevel.get(p);
        if (pLvl !== undefined && lvl <= pLvl) {
          lvl = pLvl + 1;
        }
      }

      while (occupiedCells.has(`${col}:${lvl}`)) {
        lvl++;
      }

      memoLevel.set(c.sha, lvl);
      occupiedCells.add(`${col}:${lvl}`);
    }

    interface CanvasNode {
      readonly id: string;
      readonly kind: "commit" | "working" | "merge" | "collapsed";
      readonly col: number;
      readonly level: number;
      row: number;
      readonly horizIndex: number;
      readonly horizCount: number;
      readonly horizTotalCount: number;
      readonly horizTopic?: string;
      readonly horizGroupId?: string;
      readonly horizExpanded?: boolean;
      readonly branchName: string;
      readonly shortHash: string;
      readonly title: string;
      readonly subtitle: string;
      readonly files: readonly string[];
      readonly parents: readonly string[];
      sessions: readonly { readonly id: string; readonly intent: string }[];
      readonly isHead: boolean;
      readonly hasConflict: boolean;
      readonly groupId?: string;
    }

    const canvasNodes: CanvasNode[] = [];
    const nodeById = new Map<string, CanvasNode>();

    const activeAgents = agents.filter((a) => {
      const isIdleShell =
        (a.agent_id.startsWith("term-") ||
          a.agent_id.startsWith("session-") ||
          a.agent_id === "editor") &&
        a.claimed_symbols.length === 0 &&
        Object.keys(a.own_blobs ?? {}).length === 0 &&
        a.head_commit === a.base_commit;
      return !isIdleShell;
    });

    for (const c of commits) {
      const col = commitCol.get(c.sha) ?? 0;
      const bObj = sortedBranches[col] ?? sortedBranches[0];
      const bName = bObj?.name ?? trunkBranch;
      const lvl = memoLevel.get(c.sha) ?? 0;

      if (c.collapsedCount && c.groupId) {
        const collapsedNode: CanvasNode = {
          id: c.sha,
          kind: "collapsed",
          col,
          level: lvl,
          row: 0,
          horizIndex: 0,
          horizCount: 1,
          horizTotalCount: 1,
          branchName: bName,
          shortHash: c.short_sha,
          title: c.subject,
          subtitle: "Click to expand",
          files: [],
          parents: c.parents.filter((p) => commitMap.has(p)),
          sessions: [],
          isHead: false,
          hasConflict: false,
          groupId: c.groupId,
        };
        canvasNodes.push(collapsedNode);
        nodeById.set(collapsedNode.id, collapsedNode);
        continue;
      }

      const isTipOfBranch = branchTipFullSha.get(bName) === c.sha;
      const isHead =
        c.refs.some((r) => r.includes("HEAD")) ||
        (bName === editorBranch && isTipOfBranch);

      const isConflictedMergeTip =
        isTipOfBranch &&
        bName === editorBranch &&
        conflicts.length > 0 &&
        c.parents.length > 1;

      const nodeFiles = isConflictedMergeTip
        ? Array.from(
            new Set([
              ...conflicts.map((x) => x.path),
              ...(bObj?.modified_files ?? []),
            ])
          )
        : isTipOfBranch
          ? (bObj?.modified_files ?? [])
          : [];

      const horizMembers = c.horizMembers ?? [];
      const totalHoriz = 1 + horizMembers.length;
      const isHorizExpanded =
        horizMembers.length > 0 && this.expandedHorizGroups.has(c.sha);
      const visibleHorizCount = isHorizExpanded ? totalHoriz : 1;

      const node: CanvasNode = {
        id: c.sha,
        kind: c.parents.length > 1 ? "merge" : "commit",
        col,
        level: lvl,
        row: 0,
        horizIndex: 0,
        horizCount: visibleHorizCount,
        horizTotalCount: totalHoriz,
        horizTopic: c.horizTopic,
        horizGroupId: horizMembers.length > 0 ? c.sha : undefined,
        horizExpanded: isHorizExpanded,
        branchName: bName,
        shortHash: c.short_sha,
        title: c.subject,
        subtitle: `${c.author} · ${this.formatRelativeTime(c.timestamp)}`,
        files: nodeFiles,
        parents: c.parents.filter((p) => commitMap.has(p)),
        sessions: [],
        isHead,
        hasConflict: isConflictedMergeTip,
      };
      canvasNodes.push(node);
      nodeById.set(node.id, node);

      if (isHorizExpanded) {
        for (let mIdx = 0; mIdx < horizMembers.length; mIdx++) {
          const member = horizMembers[mIdx];
          if (!member) {
            continue;
          }

          const prevSha =
            mIdx === 0 ? c.sha : (horizMembers[mIdx - 1]?.sha ?? c.sha);
          const memberNode: CanvasNode = {
            id: member.sha,
            kind: "commit",
            col,
            level: lvl,
            row: 0,
            horizIndex: mIdx + 1,
            horizCount: visibleHorizCount,
            horizTotalCount: totalHoriz,
            horizTopic: c.horizTopic,
            branchName: bName,
            shortHash: member.short_sha,
            title: member.subject,
            subtitle: `${member.author} · ${this.formatRelativeTime(member.timestamp)}`,
            files: [],
            parents: [prevSha],
            sessions: [],
            isHead: false,
            hasConflict: false,
          };
          canvasNodes.push(memberNode);
          nodeById.set(memberNode.id, memberNode);
        }
      }
    }

    for (let col = 0; col < sortedBranches.length; col++) {
      const b = sortedBranches[col];
      if (!b) {
        continue;
      }

      const bAgents = activeAgents.filter((a) => (a.target_branch ?? trunkBranch) === b.name);
      const uncommittedAgents = bAgents.filter(
        (a) =>
          Object.keys(a.own_blobs ?? {}).length > 0 ||
          (a.modified_files?.length ?? 0) > 0 ||
          (a.claimed_symbols?.length ?? 0) > 0
      );
      const rawTipSha = branchTipFullSha.get(b.name);
      const spineTipSha = rawTipSha
        ? (replacedByHorizAnchor.get(rawTipSha) ?? rawTipSha)
        : undefined;
      const tipSha = spineTipSha
        ? (replacedByCollapsedId.get(spineTipSha) ?? spineTipSha)
        : undefined;
      const tipNode = tipSha ? nodeById.get(tipSha) : undefined;
      const hasConflictOnBranch =
        conflicts.length > 0 &&
        b.name === editorBranch &&
        !tipNode?.hasConflict;

      if (uncommittedAgents.length > 0 || hasConflictOnBranch) {
        const parentSha = tipSha;
        const parentLvl = parentSha ? (memoLevel.get(parentSha) ?? 0) : 0;
        let lvl = parentLvl + 1;
        while (occupiedCells.has(`${col}:${lvl}`)) {
          lvl++;
        }

        occupiedCells.add(`${col}:${lvl}`);

        const claimedFiles = Array.from(
          new Set([
            ...(hasConflictOnBranch ? conflicts.map((c) => c.path) : []),
            ...b.modified_files,
            ...uncommittedAgents.flatMap((a) => Object.keys(a.own_blobs ?? {})),
            ...uncommittedAgents.flatMap((a) =>
              (a.claimed_symbols ?? [])
                .map((s) => s.split("::")[0]?.trim() ?? "")
                .filter(Boolean)
            ),
          ])
        ).slice(0, 4);

        const workNode: CanvasNode = {
          id: `working:${b.name}`,
          kind: "working",
          col,
          level: lvl,
          row: 0,
          horizIndex: 0,
          horizCount: 1,
          horizTotalCount: 1,
          branchName: b.name,
          shortHash: "Working",
          title: uncommittedAgents[0]?.intent ?? `Working on ${b.name}`,
          subtitle: `${uncommittedAgents.length} active session(s)`,
          files: claimedFiles,
          parents: parentSha ? [parentSha] : [],
          sessions: uncommittedAgents.map((a) => ({
            id: a.agent_id,
            intent: a.intent,
          })),
          isHead: b.name === editorBranch,
          hasConflict: hasConflictOnBranch,
        };
        canvasNodes.push(workNode);
        nodeById.set(workNode.id, workNode);
      } else {
        const hasNodeInColumn = canvasNodes.some((n) => n.col === col);
        if (!hasNodeInColumn && b.name !== trunkBranch) {
          const parentLvl = tipSha ? (memoLevel.get(tipSha) ?? 0) : 0;
          let lvl = parentLvl + 1;
          while (occupiedCells.has(`${col}:${lvl}`)) {
            lvl++;
          }

          occupiedCells.add(`${col}:${lvl}`);
          const forkNode: CanvasNode = {
            id: `branch-tip:${b.name}`,
            kind: "commit",
            col,
            level: lvl,
            row: 0,
            horizIndex: 0,
            horizCount: 1,
            horizTotalCount: 1,
            branchName: b.name,
            shortHash: b.short_commit || "HEAD",
            title: `Branched from ${trunkBranch}`,
            subtitle: `Port :${b.port}`,
            files: [],
            parents: tipSha ? [tipSha] : [],
            sessions: bAgents.map((a) => ({
              id: a.agent_id,
              intent: a.intent,
            })),
            isHead: b.name === editorBranch,
            hasConflict: false,
          };
          canvasNodes.push(forkNode);
          nodeById.set(forkNode.id, forkNode);
        } else if (tipNode && bAgents.length > 0) {
          tipNode.sessions = bAgents.map((a) => ({
            id: a.agent_id,
            intent: a.intent,
          }));
        }
      }
    }

    // Compact used levels into contiguous row indices (0 .. numRows - 1).
    const distinctLevels = Array.from(new Set(canvasNodes.map((n) => n.level))).sort(
      (a, b) => a - b
    );
    const levelToRow = new Map<number, number>();
    distinctLevels.forEach((lvl, idx) => levelToRow.set(lvl, idx));
    for (const n of canvasNodes) {
      n.row = levelToRow.get(n.level) ?? 0;
    }

    const numRows = Math.max(distinctLevels.length, 1);

    const HEADER_TOP = 10;
    const HEADER_H = 52;
    const HEADER_HEIGHT = HEADER_TOP + HEADER_H + 14;
    const PAD_X = 28;
    const PAD_Y = 12;
    const CARD_W = 244;
    const CARD_H = 82;
    const COLLAPSED_H = 36;
    const H_GAP = 18;
    const COL_PAD_X = 22;
    const ROW_GAP = 22;

    const colWidths: number[] = [];
    const colLefts: number[] = [];
    let currentX = PAD_X;
    for (let col = 0; col < numCols; col++) {
      const colNodes = canvasNodes.filter((n) => n.col === col);
      const maxHoriz = colNodes.reduce((acc, n) => Math.max(acc, n.horizCount), 1);
      const w = Math.max(maxHoriz * CARD_W + (maxHoriz - 1) * H_GAP + COL_PAD_X * 2, 288);
      colLefts.push(currentX);
      colWidths.push(w);
      currentX += w;
    }

    const rowHeights: number[] = [];
    const rowTops: number[] = [];
    let currentY = HEADER_HEIGHT + PAD_Y;
    for (let r = 0; r < numRows; r++) {
      const nodesInRow = canvasNodes.filter((n) => n.row === r);
      const isCollapsedOnlyRow =
        nodesInRow.length > 0 && nodesInRow.every((n) => n.kind === "collapsed");
      const h = isCollapsedOnlyRow ? COLLAPSED_H : CARD_H;
      rowTops.push(currentY);
      rowHeights.push(h);
      currentY += h + ROW_GAP;
    }

    const canvasWidth = currentX + PAD_X;
    const canvasHeight = Math.max(currentY - ROW_GAP + PAD_Y, HEADER_HEIGHT + 120);

    // Fix each branch column's vertical spine at a constant X offset from `colLefts[col]`
    // so every spine node (`horizIndex === 0`) and branch header aligns on a 100% straight vertical line,
    // and expanded related commits fan out strictly to the right (`horizIndex = 1, 2, ...`).
    const colSpineCenterX = (col: number): number =>
      (colLefts[col] ?? PAD_X) + COL_PAD_X + CARD_W / 2;
    const rowCenterY = (row: number): number =>
      (rowTops[row] ?? HEADER_HEIGHT + PAD_Y) + (rowHeights[row] ?? CARD_H) / 2;
    const nodeCardHeight = (n: CanvasNode): number =>
      n.kind === "collapsed" ? COLLAPSED_H : CARD_H;
    const nodeCenterX = (n: CanvasNode): number =>
      colSpineCenterX(n.col) + n.horizIndex * (CARD_W + H_GAP);

    const svgElements: string[] = [];

    // Connect each Branch Header straight down to the first spine commit in its column.
    const headerBottomY = HEADER_TOP + HEADER_H;
    for (let col = 0; col < sortedBranches.length; col++) {
      const colNodes = canvasNodes.filter((n) => n.col === col && n.horizIndex === 0);
      if (colNodes.length === 0) {
        continue;
      }

      const firstNode = colNodes.reduce((a, b) => (a.row < b.row ? a : b));
      const spineX = colSpineCenterX(col);
      const firstNodeTopY = rowCenterY(firstNode.row) - nodeCardHeight(firstNode) / 2;
      svgElements.push(
        `<line class="track-line" x1="${spineX}" y1="${headerBottomY}" x2="${spineX}" y2="${firstNodeTopY}" />`
      );
    }

    // Connect parent commits to child commits (straight vertical along the spine, or straight horizontal inside an expanded related group).
    for (const node of canvasNodes) {
      const childX = nodeCenterX(node);
      const childY = rowCenterY(node.row);
      const childTopY = childY - nodeCardHeight(node) / 2;

      if (node.horizIndex > 0 && node.parents.length === 1) {
        const prevId = node.parents[0];
        const prevNode = prevId ? nodeById.get(prevId) : undefined;
        if (prevNode && prevNode.row === node.row) {
          const x1 = nodeCenterX(prevNode) + CARD_W / 2;
          const x2 = childX - CARD_W / 2;
          const y = childY;
          svgElements.push(
            `<path class="edge-line horiz" d="M ${x1} ${y} L ${x2} ${y}" />`
          );
          continue;
        }
      }

      for (let pIdx = 0; pIdx < node.parents.length; pIdx++) {
        const parentId = node.parents[pIdx];
        if (!parentId) {
          continue;
        }

        const parentNode = nodeById.get(parentId);
        if (!parentNode) {
          continue;
        }

        const parentX = nodeCenterX(parentNode);
        const parentY = rowCenterY(parentNode.row);
        const parentBottomY = parentY + nodeCardHeight(parentNode) / 2;

        const edgeClass =
          node.kind === "working"
            ? "edge-line dashed"
            : pIdx > 0 || node.col !== parentNode.col
              ? "edge-line branch"
              : "edge-line";

        if (parentX === childX) {
          svgElements.push(
            `<path class="${edgeClass}" d="M ${parentX} ${parentBottomY} L ${childX} ${childTopY}" />`
          );
        } else {
          const dy = Math.max(Math.abs(childTopY - parentBottomY) * 0.5, 16);
          const d = `M ${parentX} ${parentBottomY} C ${parentX} ${parentBottomY + dy}, ${childX} ${childTopY - dy}, ${childX} ${childTopY}`;
          svgElements.push(`<path class="${edgeClass}" d="${d}" />`);
        }
      }
    }

    const colHeaderHtml = sortedBranches
      .map((b, col) => {
        const cx = colSpineCenterX(col);
        const left = cx - CARD_W / 2;
        const isViewing = b.name === editorBranch;
        const canDelete = b.name !== trunkBranch;
        return `
        <div class="col-header ${isViewing ? "viewing" : ""}" style="left: ${left}px; width: ${CARD_W}px; height: ${HEADER_H}px;" data-action="switchBranch" data-branch="${this.escapeHtml(b.name)}" title="Switch editor to '${this.escapeHtml(b.name)}'">
          <div class="col-header-top">
            <span class="col-name">${this.escapeHtml(b.name)}</span>
            <div class="col-header-actions">
              ${
                isViewing
                  ? '<span class="badge">Active</span>'
                  : '<span class="badge secondary">Switch</span>'
              }
              ${
                canDelete
                  ? `<button class="icon-btn" data-action="deleteBranch" data-branch="${this.escapeHtml(b.name)}" title="Delete branch '${this.escapeHtml(b.name)}'">×</button>`
                  : ""
              }
            </div>
          </div>
          <div class="col-header-sub">
            <span>${b.name === trunkBranch ? "Default" : "Branch"}</span>
            <span>:${b.port}</span>
          </div>
        </div>`;
      })
      .join("");

    const nodesHtml = canvasNodes
      .map((n) => {
        const cx = nodeCenterX(n);
        const cy = rowCenterY(n.row);
        const cardH = nodeCardHeight(n);
        const left = cx - CARD_W / 2;
        const top = cy - cardH / 2;

        if (n.kind === "collapsed" && n.groupId) {
          return `
          <button class="graph-node kind-collapsed"
                  style="left: ${left}px; top: ${top}px; width: ${CARD_W}px; height: ${cardH}px;"
                  data-action="expandGroup"
                  data-group-id="${this.escapeHtml(n.groupId)}"
                  title="Expand ${this.escapeHtml(n.shortHash)} (${this.escapeHtml(n.title)})">
            <span class="collapsed-pill-badge">${this.escapeHtml(n.shortHash)}</span>
            <span class="collapsed-pill-range">${this.escapeHtml(n.title)}</span>
            <span class="collapsed-pill-action">Expand</span>
          </button>`;
        }

        const fileAction = n.hasConflict ? "openFile" : "openBranchDiff";
        const fileChips = n.files
          .slice(0, 3)
          .map(
            (f) => `
            <button class="file-chip" data-action="${fileAction}" data-path="${this.escapeHtml(f)}" data-branch="${this.escapeHtml(n.branchName)}" title="${this.escapeHtml(f)}">
              ${this.escapeHtml(this.basename(f))}
            </button>`
          )
          .join("");

        const agentIconSvg = `<svg class="agent-icon" width="11" height="11" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="10" height="8" rx="2"/><path d="M8 2v3"/><circle cx="6" cy="9" r="0.75" fill="currentColor"/><circle cx="10" cy="9" r="0.75" fill="currentColor"/></svg>`;

        let sessionBadges = "";

        if (n.sessions.length > 0) {
          const singleShortName = this.shortAgentName(n.sessions[0].id);
          const maxSingleChars = n.isHead ? 10 : 15;
          const hasOverflowRisk =
            n.sessions.length > 1 || singleShortName.length > maxSingleChars;

          const badgeLabel = hasOverflowRisk
            ? `+${n.sessions.length}`
            : this.escapeHtml(singleShortName);

          const fallbackTitle = n.sessions
            .map((s) => `${this.shortAgentName(s.id)}: ${s.intent || "Active session"}`)
            .join("\n");

          sessionBadges = `
            <span class="badge secondary agent-badge" title="${this.escapeHtml(fallbackTitle)}">
              ${agentIconSvg}
              <span class="agent-badge-text">${badgeLabel}</span>
            </span>`;
        }

        const kindLabel =
          n.kind === "merge"
            ? `<span class="badge">Merge</span>`
            : n.kind === "working"
              ? `<span class="badge">${this.escapeHtml(n.shortHash)}</span>`
              : `<span class="node-hash">${this.escapeHtml(n.shortHash)}</span>`;

        const horizToggleBtn =
          n.horizIndex === 0 && n.horizTotalCount > 1 && n.horizGroupId
            ? `<button class="file-chip horiz-toggle" data-action="toggleHorizGroup" data-group-id="${this.escapeHtml(n.horizGroupId)}" title="${n.horizExpanded ? "Collapse" : "Expand"} ${n.horizTotalCount - 1} related ${this.escapeHtml(n.horizTopic ?? "")} commit(s)">${
                n.horizExpanded
                  ? `◂ Hide ${n.horizTotalCount - 1}`
                  : `+${n.horizTotalCount - 1} ${this.escapeHtml(n.horizTopic ?? "related")} ▸`
              }</button>`
            : n.horizIndex > 0 && n.horizTopic
              ? `<span class="badge secondary">${this.escapeHtml(n.horizTopic)} ${n.horizIndex + 1}/${n.horizTotalCount}</span>`
              : "";

        const discardBtn =
          n.kind === "working"
            ? `<button class="file-chip" data-action="discardWorking" data-branch="${this.escapeHtml(n.branchName)}" title="Discard uncommitted working changes on '${this.escapeHtml(n.branchName)}'">Discard</button>`
            : "";

        return `
        <div class="graph-node kind-${n.kind} ${n.isHead ? "is-head" : ""} ${n.hasConflict ? "is-conflict" : ""}"
             style="left: ${left}px; top: ${top}px; width: ${CARD_W}px; height: ${cardH}px;">
          <div class="node-top-bar">
            <div class="node-id-group">
              ${kindLabel}
              ${n.isHead ? '<span class="badge">HEAD</span>' : ""}
              ${horizToggleBtn}
            </div>
            <div class="node-agents">${sessionBadges}</div>
          </div>
          <div class="node-title" title="${this.escapeHtml(n.title)}">${this.escapeHtml(n.title)}</div>
          <div class="node-footer">
            <div class="node-files">${fileChips || `<span class="node-meta">${this.escapeHtml(n.subtitle)}</span>`}</div>
            ${discardBtn}
          </div>
        </div>`;
      })
      .join("");

    const mergedIntoTrunk = new Set<string>();
    if (trunkTip) {
      const queue = [trunkTip];
      while (queue.length > 0) {
        const cur = queue.pop();
        if (!cur || mergedIntoTrunk.has(cur)) {
          continue;
        }

        mergedIntoTrunk.add(cur);
        const node = rawCommitMap.get(cur);
        if (node) {
          for (const p of node.parents) {
            queue.push(p);
          }
        }
      }
    }

    const mergeableBranches = sortedBranches
      .filter((b) => {
        if (b.name === trunkBranch) {
          return false;
        }

        const bTip = branchTipFullSha.get(b.name);
        const hasWorkingNode = nodeById.has(`working:${b.name}`);
        const isAlreadyMerged = bTip ? mergedIntoTrunk.has(bTip) : false;
        return !isAlreadyMerged || hasWorkingNode;
      })
      .map((b) => b.name);

    const firstConflict: ConflictItem | undefined = conflicts[0];
    const hasAnyExpanded =
      this.expandAll ||
      this.expandedGroups.size > 0 ||
      this.expandedHorizGroups.size > 0 ||
      (totalCollapsibleGroups > 0 && collapsedNodesByNewest.size === 0);
    const hasAnyCollapsed = collapsedNodesByNewest.size > 0;

    const nonce = crypto.randomBytes(16).toString("base64");

    return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; img-src data:; script-src 'nonce-${nonce}';">
<style>
  * { box-sizing: border-box; }
  html, body {
    margin: 0;
    padding: 0;
    width: 100%;
    height: 100vh;
    overflow: hidden;
    display: flex;
    flex-direction: column;
    background-color: var(--vscode-editor-background);
    color: var(--vscode-foreground);
    font-family: var(--vscode-font-family);
    font-size: var(--vscode-font-size, 13px);
  }
  .topbar {
    flex-shrink: 0;
    width: 100%;
    z-index: 20;
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 10px 20px;
    background-color: var(--vscode-editor-background);
    border-bottom: 1px solid var(--vscode-panel-border, var(--vscode-widget-border));
  }
  .topbar-left {
    display: flex;
    align-items: center;
    gap: 12px;
  }
  .topbar-title {
    font-size: 13px;
    font-weight: 600;
  }
  .topbar-sub {
    font-size: 12px;
    color: var(--vscode-descriptionForeground);
  }
  .topbar-actions {
    display: flex;
    gap: 6px;
  }
  .btn {
    background-color: var(--vscode-button-secondaryBackground);
    color: var(--vscode-button-secondaryForeground);
    border: 1px solid var(--vscode-contrastBorder, transparent);
    border-radius: 2px;
    padding: 4px 10px;
    font-family: var(--vscode-font-family);
    font-size: 12px;
    cursor: pointer;
  }
  .btn:hover {
    background-color: var(--vscode-button-secondaryHoverBackground);
  }
  .btn.primary {
    background-color: var(--vscode-button-background);
    color: var(--vscode-button-foreground);
  }
  .btn.primary:hover {
    background-color: var(--vscode-button-hoverBackground);
  }
  .btn.conflict {
    background-color: var(--vscode-inputValidation-errorBackground, var(--vscode-button-secondaryBackground));
    color: var(--vscode-errorForeground, var(--vscode-foreground));
    border: 1px solid var(--vscode-inputValidation-errorBorder, var(--vscode-errorForeground));
  }
  .graph-viewport {
    flex: 1;
    width: 100%;
    min-height: 0;
    overflow: auto;
    position: relative;
  }
  .graph-stage {
    position: relative;
    margin: 0 auto 24px;
  }
  .col-header-bar {
    position: sticky;
    top: 0;
    z-index: 10;
    height: ${HEADER_HEIGHT}px;
    background-color: var(--vscode-editor-background);
  }
  .graph-svg {
    position: absolute;
    top: 0;
    left: 0;
    pointer-events: none;
    z-index: 1;
  }
  .track-line {
    stroke: var(--vscode-tree-indentGuidesStroke, var(--vscode-panel-border));
    stroke-width: 1;
    stroke-dasharray: 4 4;
  }
  .edge-line {
    fill: none;
    stroke: var(--vscode-descriptionForeground);
    stroke-width: 1.5;
  }
  .edge-line.branch {
    stroke: var(--vscode-descriptionForeground);
  }
  .edge-line.dashed {
    stroke-dasharray: 4 4;
  }
  .edge-arrow {
    fill: none;
    stroke: var(--vscode-descriptionForeground);
    stroke-width: 1.5;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
  .col-header {
    position: absolute;
    top: ${HEADER_TOP}px;
    z-index: 5;
    background-color: var(--vscode-sideBar-background, var(--vscode-editorWidget-background));
    border: 1px solid var(--vscode-widget-border, var(--vscode-panel-border));
    border-radius: 2px;
    padding: 7px 10px;
    cursor: pointer;
  }
  .col-header:hover {
    background-color: var(--vscode-list-hoverBackground);
  }
  .col-header.viewing {
    border-color: var(--vscode-focusBorder);
  }
  .col-header-top {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 6px;
  }
  .col-header-actions {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    flex-shrink: 0;
  }
  .icon-btn {
    background: transparent;
    color: var(--vscode-descriptionForeground);
    border: 1px solid transparent;
    border-radius: 2px;
    padding: 0 4px;
    font-size: 12px;
    line-height: 15px;
    cursor: pointer;
  }
  .icon-btn:hover {
    background-color: var(--vscode-toolbar-hoverBackground, var(--vscode-button-secondaryHoverBackground));
    color: var(--vscode-foreground);
  }
  .col-name {
    font-size: 12px;
    font-weight: 600;
    font-family: var(--vscode-editor-font-family);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    flex: 1;
  }
  .col-header-sub {
    display: flex;
    justify-content: space-between;
    margin-top: 3px;
    font-size: 11px;
    color: var(--vscode-descriptionForeground);
  }
  .badge {
    font-size: 10px;
    font-weight: 500;
    padding: 1px 5px;
    border-radius: 2px;
    background-color: var(--vscode-badge-background);
    color: var(--vscode-badge-foreground);
    white-space: nowrap;
  }
  .badge.secondary {
    background-color: var(--vscode-button-secondaryBackground);
    color: var(--vscode-button-secondaryForeground);
  }
  .graph-node {
    position: absolute;
    z-index: 4;
    background-color: var(--vscode-editorWidget-background, var(--vscode-sideBar-background));
    border: 1px solid var(--vscode-widget-border, var(--vscode-panel-border));
    border-radius: 2px;
    padding: 6px 10px;
    display: flex;
    flex-direction: column;
    justify-content: space-between;
  }
  .graph-node:hover {
    background-color: var(--vscode-list-hoverBackground);
  }
  .graph-node.is-head {
    border-color: var(--vscode-focusBorder);
  }
  .graph-node.kind-working {
    border-style: dashed;
  }
  .graph-node.is-conflict {
    border-color: var(--vscode-inputValidation-errorBorder, var(--vscode-errorForeground));
  }
  .graph-node.kind-collapsed {
    flex-direction: row;
    align-items: center;
    justify-content: space-between;
    gap: 6px;
    padding: 4px 10px;
    border-style: dashed;
    cursor: pointer;
    font-family: var(--vscode-font-family);
    color: var(--vscode-foreground);
    text-align: left;
  }
  .collapsed-pill-badge {
    font-family: var(--vscode-editor-font-family);
    font-size: 11px;
    font-weight: 600;
    color: var(--vscode-badge-foreground);
    background-color: var(--vscode-badge-background);
    padding: 1px 6px;
    border-radius: 2px;
    white-space: nowrap;
    flex-shrink: 0;
  }
  .collapsed-pill-range {
    font-family: var(--vscode-editor-font-family);
    font-size: 11px;
    color: var(--vscode-descriptionForeground);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    flex: 1;
    min-width: 0;
  }
  .collapsed-pill-action {
    font-size: 11px;
    color: var(--vscode-textLink-foreground, var(--vscode-descriptionForeground));
    white-space: nowrap;
    flex-shrink: 0;
  }
  .node-top-bar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 6px;
    min-width: 0;
  }
  .node-id-group {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-shrink: 0;
  }
  .node-agents {
    display: flex;
    align-items: center;
    gap: 4px;
    min-width: 0;
    flex-shrink: 1;
  }
  .agent-badge {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    max-width: 100%;
    overflow: hidden;
    cursor: default;
  }
  .agent-icon {
    flex-shrink: 0;
    display: block;
  }
  .agent-badge-text {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .node-hash {
    font-family: var(--vscode-editor-font-family);
    font-size: 11px;
    color: var(--vscode-descriptionForeground);
    background-color: var(--vscode-textCodeBlock-background);
    padding: 1px 5px;
    border-radius: 2px;
  }
  .node-title {
    font-size: 12px;
    font-weight: 500;
    line-height: 1.28;
    color: var(--vscode-foreground);
    display: -webkit-box;
    -webkit-line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
  .node-footer {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 4px;
    min-width: 0;
  }
  .node-files {
    display: flex;
    gap: 4px;
    flex-wrap: nowrap;
    overflow: hidden;
    min-width: 0;
    max-width: 100%;
  }
  .file-chip {
    background-color: var(--vscode-button-secondaryBackground);
    color: var(--vscode-button-secondaryForeground);
    border: 1px solid var(--vscode-contrastBorder, transparent);
    border-radius: 2px;
    padding: 1px 6px;
    font-size: 11px;
    font-family: var(--vscode-editor-font-family);
    cursor: pointer;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    min-width: 0;
  }
  .file-chip:hover {
    background-color: var(--vscode-button-secondaryHoverBackground);
  }
  .node-meta {
    font-size: 11px;
    color: var(--vscode-descriptionForeground);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
</style>
</head>
<body>
  <div class="topbar">
    <div class="topbar-left">
      <span class="topbar-title">Branch Graph</span>
      <span class="topbar-sub">Active: ${this.escapeHtml(editorBranch)}</span>
    </div>
    <div class="topbar-actions">
      ${
        firstConflict
          ? `<button class="btn conflict" data-action="openFile" data-path="${this.escapeHtml(firstConflict.path)}">Resolve Conflict (${this.escapeHtml(this.basename(firstConflict.path))})</button>`
          : ""
      }
      ${
        mergeableBranches.length > 0
          ? `<button class="btn primary" data-action="mergeInto" data-sources="${this.escapeHtml(mergeableBranches.join(","))}" data-target="${this.escapeHtml(trunkBranch)}">Merge into ${this.escapeHtml(trunkBranch)}</button>`
          : ""
      }
      ${
        hasAnyExpanded
          ? `<button class="btn" data-action="collapseAll">Collapse Linear</button>`
          : ""
      }
      ${
        hasAnyCollapsed
          ? `<button class="btn" data-action="expandAll">Expand All</button>`
          : ""
      }
      <button class="btn" data-action="refresh">Refresh</button>
    </div>
  </div>

  <div class="graph-viewport">
    <div class="graph-stage" style="width: ${canvasWidth}px; height: ${canvasHeight}px;">
      <svg class="graph-svg" width="${canvasWidth}" height="${canvasHeight}" viewBox="0 0 ${canvasWidth} ${canvasHeight}">
        ${svgElements.join("\n        ")}
      </svg>
      <div class="col-header-bar" style="width: ${canvasWidth}px;">
        ${colHeaderHtml}
      </div>
      ${nodesHtml}
    </div>
  </div>

  <script nonce="${nonce}">
    const vscode = acquireVsCodeApi();
    document.addEventListener('click', (ev) => {
      const target = ev.target.closest('[data-action]');
      if (!target) return;
      ev.stopPropagation();
      const action = target.getAttribute('data-action');
      if (action === 'switchBranch') {
        vscode.postMessage({
          command: 'switchBranch',
          branch: target.getAttribute('data-branch'),
        });
      } else if (action === 'expandGroup') {
        vscode.postMessage({
          command: 'expandGroup',
          groupId: target.getAttribute('data-group-id'),
        });
      } else if (action === 'toggleHorizGroup') {
        vscode.postMessage({
          command: 'toggleHorizGroup',
          groupId: target.getAttribute('data-group-id'),
        });
      } else if (action === 'collapseAll') {
        vscode.postMessage({
          command: 'collapseAll',
        });
      } else if (action === 'expandAll') {
        vscode.postMessage({
          command: 'expandAll',
        });
      } else if (action === 'mergeInto') {
        const raw = target.getAttribute('data-sources') || '';
        vscode.postMessage({
          command: 'mergeInto',
          sources: raw.split(',').filter(Boolean),
          target: target.getAttribute('data-target') || 'main',
        });
      } else if (action === 'openBranchDiff') {
        vscode.postMessage({
          command: 'openBranchDiff',
          filePath: target.getAttribute('data-path'),
          branch: target.getAttribute('data-branch'),
        });
      } else if (action === 'openFile') {
        vscode.postMessage({
          command: 'openFile',
          filePath: target.getAttribute('data-path'),
        });
      } else if (action === 'resolveConflict') {
        vscode.postMessage({
          command: 'resolveConflict',
          filePath: target.getAttribute('data-path'),
          strategy: target.getAttribute('data-strategy'),
        });
      } else if (action === 'deleteBranch') {
        vscode.postMessage({
          command: 'deleteBranch',
          branch: target.getAttribute('data-branch'),
        });
      } else if (action === 'discardWorking') {
        vscode.postMessage({
          command: 'discardWorking',
          branch: target.getAttribute('data-branch'),
        });
      } else if (action === 'refresh') {
        vscode.postMessage({ command: 'refresh' });
      }
    });
  </script>
</body>
</html>`;
  }

  /**
   * Extracts the normalized conventional-commit `type(scope)` or `type` key from a commit subject
   * so only genuinely related consecutive commits (e.g., `docs`, `feat(vscode)`, `test(e2e)`) are grouped.
   *
   * @param subject The raw commit subject line.
   * @returns The normalized topic key, or an empty string if not a conventional commit.
   */
  private extractCommitTopic(subject: string): string {
    const trimmed = subject.trim();
    const match = /^([a-zA-Z]+)(?:\(([^)]+)\))?!?:/.exec(trimmed);

    if (!match) {
      return "";
    }

    const rawType = (match[1] ?? "").trim().toLowerCase();
    const rawScope = (match[2] ?? "")
      .trim()
      .toLowerCase()
      .replace(/\s+/g, "");

    if (!rawType) {
      return "";
    }

    return rawScope ? `${rawType}(${rawScope})` : rawType;
  }

  /**
   * Returns `true` when two consecutive commits share the exact same conventional `type(scope)` key.
   *
   * @param topicA The first normalized commit topic.
   * @param topicB The second normalized commit topic.
   * @returns True if both topics are non-empty and identical.
   */
  private areCommitTopicsRelated(topicA: string, topicB: string): boolean {
    return Boolean(topicA && topicB && topicA === topicB);
  }

  /**
   * Formats a Unix epoch timestamp (in seconds) as a concise relative age string.
   *
   * @param timestampSeconds The Unix timestamp in seconds.
   * @returns A relative time string (e.g., `"5m ago"`, `"2h ago"`).
   */
  private formatRelativeTime(timestampSeconds: number): string {
    if (!timestampSeconds) {
      return "just now";
    }

    const diffSec = Math.max(
      0,
      Math.floor(Date.now() / 1000) - timestampSeconds
    );

    if (diffSec < 60) {
      return `${diffSec}s ago`;
    }

    if (diffSec < 3600) {
      return `${Math.floor(diffSec / 60)}m ago`;
    }

    if (diffSec < 86400) {
      return `${Math.floor(diffSec / 3600)}h ago`;
    }

    return `${Math.floor(diffSec / 86400)}d ago`;
  }

  /**
   * Abbreviates long session identifiers so they fit cleanly inside compact node badges.
   *
   * @param id The full session or agent identifier.
   * @returns An abbreviated display name.
   */
  private shortAgentName(id: string): string {
    if (id.startsWith("antigravity-")) {
      return `agy-${id.slice("antigravity-".length, "antigravity-".length + 6)}`;
    }

    return id.length > 14 ? id.slice(0, 14) : id;
  }

  /**
   * Extracts the trailing filename component from a slash-delimited relative path.
   *
   * @param filePath The slash-delimited relative file path.
   * @returns The basename of the path.
   */
  private basename(filePath: string): string {
    const parts = filePath.split("/");
    return parts[parts.length - 1] ?? filePath;
  }

  /**
   * Escapes HTML special characters for safe interpolation into webview markup and attributes.
   *
   * @param raw The unescaped input string.
   * @returns The HTML-escaped string.
   */
  private escapeHtml(raw: string): string {
    return raw
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }
}
