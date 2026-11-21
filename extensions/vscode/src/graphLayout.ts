/**
 * Pure 2D topological DAG layout engine for the Branch Graph webview.
 * Computes branch columns, horizontal topic groups, collapsed linear chains, and newest-first rows.
 */

import {
  AgentEntry,
  BranchDetail,
  CommitGraphNode,
  ConflictItem,
  WorkspaceStateSnapshot,
} from "./client";

/**
 * Interactive graph expansion state controlled by the user.
 */
export interface GraphLayoutOptions {
  readonly expandAll: boolean;
  readonly expandedGroups: ReadonlySet<string>;
  readonly expandedHorizGroups: ReadonlySet<string>;
}

/**
 * Positioned commit, merge, working, or collapsed-chain card on the 2D graph canvas.
 */
export interface CanvasNode {
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

/**
 * Complete geometry and metadata computed by `computeGraphLayout`.
 */
export interface GraphLayoutResult {
  readonly editorBranch: string;
  readonly trunkBranch: string;
  readonly sortedBranches: readonly BranchDetail[];
  readonly canvasNodes: readonly CanvasNode[];
  readonly nodeById: ReadonlyMap<string, CanvasNode>;
  readonly colLefts: readonly number[];
  readonly colWidths: readonly number[];
  readonly rowTops: readonly number[];
  readonly rowHeights: readonly number[];
  readonly canvasWidth: number;
  readonly canvasHeight: number;
  readonly headerTop: number;
  readonly headerH: number;
  readonly headerHeight: number;
  readonly padX: number;
  readonly cardW: number;
  readonly cardH: number;
  readonly collapsedH: number;
  readonly hGap: number;
  readonly colPadX: number;
  readonly mergeableBranches: readonly string[];
  readonly firstConflict: ConflictItem | undefined;
  readonly hasAnyExpanded: boolean;
  readonly hasAnyCollapsed: boolean;
}

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

/**
 * Extracts the normalized conventional-commit `type(scope)` or `type` key from a commit subject
 * so only genuinely related consecutive commits (for example, `docs`, `feat(vscode)`) are grouped.
 *
 * @param subject The raw commit subject line.
 * @returns The normalized topic key, or an empty string if not a conventional commit.
 */
export function extractCommitTopic(subject: string): string {
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
export function areCommitTopicsRelated(
  topicA: string,
  topicB: string
): boolean {
  return Boolean(topicA && topicB && topicA === topicB);
}

/**
 * Formats a Unix epoch timestamp (in seconds) as a concise relative age string.
 *
 * @param timestampSeconds The Unix timestamp in seconds.
 * @returns A relative time string (for example, `"5m ago"`, `"2h ago"`).
 */
export function formatRelativeTime(timestampSeconds: number): string {
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
 * Computes the horizontal spine center X coordinate for `col`.
 *
 * @param layout The computed graph layout geometry.
 * @param col Zero-based branch column index.
 * @returns The X coordinate in pixels for the column's vertical spine.
 */
export function colSpineCenterX(
  layout: GraphLayoutResult,
  col: number
): number {
  return (layout.colLefts[col] ?? layout.padX) + layout.colPadX + layout.cardW / 2;
}

/**
 * Computes the vertical center Y coordinate for `row`.
 *
 * @param layout The computed graph layout geometry.
 * @param row Zero-based row index (0 is the topmost newest row).
 * @returns The Y coordinate in pixels for the row's center.
 */
export function rowCenterY(layout: GraphLayoutResult, row: number): number {
  return (
    (layout.rowTops[row] ?? layout.headerHeight + 12) +
    (layout.rowHeights[row] ?? layout.cardH) / 2
  );
}

/**
 * Returns the pixel height of a single canvas node card.
 *
 * @param layout The computed graph layout geometry.
 * @param node The canvas node to measure.
 * @returns Height in pixels.
 */
export function nodeCardHeight(
  layout: GraphLayoutResult,
  node: CanvasNode
): number {
  return node.kind === "collapsed" ? layout.collapsedH : layout.cardH;
}

/**
 * Computes the horizontal center X coordinate for `node`, accounting for horizontal fan-out.
 *
 * @param layout The computed graph layout geometry.
 * @param node The canvas node to position.
 * @returns The X coordinate in pixels.
 */
export function nodeCenterX(
  layout: GraphLayoutResult,
  node: CanvasNode
): number {
  return (
    colSpineCenterX(layout, node.col) +
    node.horizIndex * (layout.cardW + layout.hGap)
  );
}

/**
 * Computes the complete 2D topological DAG layout for `snapshot`, ordering rows newest-first
 * so `Working` and `HEAD` remain visible at the top beneath the sticky branch headers.
 *
 * @param snapshot The repository state snapshot to lay out.
 * @param options User expansion/collapse state for linear and horizontal groups.
 * @returns The computed `GraphLayoutResult` ready for HTML/SVG rendering.
 */
export function computeGraphLayout(
  snapshot: WorkspaceStateSnapshot,
  options: GraphLayoutOptions
): GraphLayoutResult {
  const { branchReport, radarReport, conflicts } = snapshot;
  const details: readonly BranchDetail[] = branchReport.branch_details;
  const rawCommits: readonly CommitGraphNode[] = branchReport.commit_graph;
  const agents: readonly AgentEntry[] = radarReport.active_agents;

  const trunkBranch =
    branchReport.trunk_branch ||
    (details.some((b) => b.name === "main")
      ? "main"
      : (details[0]?.name ?? "main"));
  const editorBranch =
    branchReport.editor_branch || branchReport.active_branch || trunkBranch;

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

  const allCommits: MutableCommit[] = rawCommits.map((c) => ({
    ...c,
    parents: [...c.parents],
    refs: [...c.refs],
  }));

  const branchTipFullSha = new Map<string, string>();

  for (const b of sortedBranches) {
    const exactMatch = allCommits.find((c) => c.sha === b.commit);

    if (exactMatch) {
      branchTipFullSha.set(b.name, exactMatch.sha);
      continue;
    }

    if (b.short_commit.length >= 7) {
      const prefixMatch = allCommits.find((c) =>
        c.sha.startsWith(b.short_commit)
      );

      if (prefixMatch) {
        branchTipFullSha.set(b.name, prefixMatch.sha);
      }
    }
  }

  const rawCommitMap = new Map<string, MutableCommit>();

  for (const c of allCommits) {
    rawCommitMap.set(c.sha, c);
  }

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

    const childTopic = extractCommitTopic(c.subject);
    const parentTopic = extractCommitTopic(parentNode.subject);

    if (areCommitTopicsRelated(childTopic, parentTopic)) {
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
    const topic = extractCommitTopic(c.subject);

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
    let cur = rawTip
      ? (replacedByHorizAnchor.get(rawTip) ?? rawTip)
      : undefined;

    for (let i = 0; i < keepRecent && cur; i++) {
      pinnedShas.add(cur);
      cur = spineCommitMap.get(cur)?.parents[0];
    }
  }

  for (const c of spineCommits) {
    const col = commitCol.get(c.sha) ?? 0;
    const children = childrenMap.get(c.sha) ?? [];
    const hasCrossColChild = children.some(
      (ch) => (commitCol.get(ch) ?? 0) !== col
    );
    const isHeadRef = c.refs.some((r) => r.includes("HEAD"));

    if (
      c.parents.length > 1 ||
      children.length > 1 ||
      hasCrossColChild ||
      isHeadRef
    ) {
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

    if (options.expandAll || options.expandedGroups.has(groupId)) {
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
      horizMembers.length > 0 && options.expandedHorizGroups.has(c.sha);
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
      subtitle: `${c.author} · ${formatRelativeTime(c.timestamp)}`,
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
          subtitle: `${member.author} · ${formatRelativeTime(member.timestamp)}`,
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

    const bAgents = activeAgents.filter(
      (a) => (a.target_branch ?? trunkBranch) === b.name
    );
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
        const safePort = Number.isFinite(Number(b.port))
          ? Math.trunc(Number(b.port))
          : 0;

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
          subtitle: `Port :${safePort}`,
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

  // Order levels descending so newest commits, HEAD, and Working sit at row 0 (top).
  const distinctLevels = Array.from(
    new Set(canvasNodes.map((n) => n.level))
  ).sort((a, b) => b - a);
  const levelToRow = new Map<number, number>();
  distinctLevels.forEach((lvl, idx) => levelToRow.set(lvl, idx));

  for (const n of canvasNodes) {
    n.row = levelToRow.get(n.level) ?? 0;
  }

  const numRows = Math.max(distinctLevels.length, 1);

  const headerTop = 10;
  const headerH = 52;
  const headerHeight = headerTop + headerH + 14;
  const padX = 28;
  const padY = 12;
  const cardW = 244;
  const cardH = 82;
  const collapsedH = 36;
  const hGap = 18;
  const colPadX = 22;
  const rowGap = 22;

  const colWidths: number[] = [];
  const colLefts: number[] = [];
  let currentX = padX;

  for (let col = 0; col < numCols; col++) {
    const colNodes = canvasNodes.filter((n) => n.col === col);
    const maxHoriz = colNodes.reduce(
      (acc, n) => Math.max(acc, n.horizCount),
      1
    );
    const w = Math.max(
      maxHoriz * cardW + (maxHoriz - 1) * hGap + colPadX * 2,
      288
    );
    colLefts.push(currentX);
    colWidths.push(w);
    currentX += w;
  }

  const rowHeights: number[] = [];
  const rowTops: number[] = [];
  let currentY = headerHeight + padY;

  for (let r = 0; r < numRows; r++) {
    const nodesInRow = canvasNodes.filter((n) => n.row === r);
    const isCollapsedOnlyRow =
      nodesInRow.length > 0 && nodesInRow.every((n) => n.kind === "collapsed");
    const h = isCollapsedOnlyRow ? collapsedH : cardH;
    rowTops.push(currentY);
    rowHeights.push(h);
    currentY += h + rowGap;
  }

  const canvasWidth = currentX + padX;
  const canvasHeight = Math.max(
    currentY - rowGap + padY,
    headerHeight + 120
  );

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

  const hasAnyExpanded =
    options.expandAll ||
    options.expandedGroups.size > 0 ||
    options.expandedHorizGroups.size > 0 ||
    (totalCollapsibleGroups > 0 && collapsedNodesByNewest.size === 0);
  const hasAnyCollapsed = collapsedNodesByNewest.size > 0;

  return {
    editorBranch,
    trunkBranch,
    sortedBranches,
    canvasNodes,
    nodeById,
    colLefts,
    colWidths,
    rowTops,
    rowHeights,
    canvasWidth,
    canvasHeight,
    headerTop,
    headerH,
    headerHeight,
    padX,
    cardW,
    cardH,
    collapsedH,
    hGap,
    colPadX,
    mergeableBranches,
    firstConflict: conflicts[0],
    hasAnyExpanded,
    hasAnyCollapsed,
  };
}
