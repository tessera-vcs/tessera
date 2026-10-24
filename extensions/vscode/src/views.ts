/**
 * Sidebar TreeView providers for branches, active sessions, conflicts, checkpoints, and stashes.
 */

import * as vscode from "vscode";
import {
  AgentEntry,
  BranchDetail,
  ConflictItem,
  WorkspaceStateSnapshot,
} from "./client";

/**
 * Node item displayed inside the sidebar TreeViews.
 */
export class SidebarNode extends vscode.TreeItem {
  public readonly branchName?: string;
  public readonly sessionId?: string;
  public readonly filePath?: string;
  public readonly conflict?: ConflictItem;
  public readonly checkpointLabel?: string;
  public readonly stashIndex?: number;
  public readonly port?: number;

  /**
   * Constructs a sidebar tree node with optional metadata and click command.
   *
   * @param label The primary display label for the tree item.
   * @param collapsibleState Whether the tree item can be expanded.
   * @param options Optional metadata, icon, tooltip, and click command.
   */
  public constructor(
    label: string,
    collapsibleState: vscode.TreeItemCollapsibleState,
    options?: {
      description?: string;
      tooltip?: string;
      contextValue?: string;
      icon?: string;
      command?: vscode.Command;
      branchName?: string;
      sessionId?: string;
      filePath?: string;
      conflict?: ConflictItem;
      checkpointLabel?: string;
      stashIndex?: number;
      port?: number;
    }
  ) {
    super(label, collapsibleState);

    if (options?.description !== undefined) {
      this.description = options.description;
    }

    if (options?.tooltip !== undefined) {
      this.tooltip = options.tooltip;
    }

    if (options?.contextValue !== undefined) {
      this.contextValue = options.contextValue;
    }

    if (options?.icon !== undefined) {
      this.iconPath = new vscode.ThemeIcon(options.icon);
    }

    if (options?.command !== undefined) {
      this.command = options.command;
    }

    this.branchName = options?.branchName;
    this.sessionId = options?.sessionId;
    this.filePath = options?.filePath;
    this.conflict = options?.conflict;
    this.checkpointLabel = options?.checkpointLabel;
    this.stashIndex = options?.stashIndex;
    this.port = options?.port;
  }
}

/**
 * TreeDataProvider for the "Branches" view.
 * Expanding a branch shows its modified files ahead of `main` and active sessions.
 */
export class BranchesTreeProvider
  implements vscode.TreeDataProvider<SidebarNode>
{
  private snapshot: WorkspaceStateSnapshot | undefined;
  private readonly onDidChangeTreeDataEmitter = new vscode.EventEmitter<
    SidebarNode | undefined | void
  >();
  public readonly onDidChangeTreeData = this.onDidChangeTreeDataEmitter.event;

  /**
   * Updates the snapshot and triggers a TreeView refresh.
   *
   * @param snapshot The latest repository state snapshot.
   */
  public setSnapshot(snapshot: WorkspaceStateSnapshot | undefined): void {
    this.snapshot = snapshot;
    this.onDidChangeTreeDataEmitter.fire();
  }

  /**
   * Returns the `TreeItem` representation for `element`.
   *
   * @param element The sidebar node to render.
   * @returns The corresponding VS Code `TreeItem`.
   */
  public getTreeItem(element: SidebarNode): vscode.TreeItem {
    return element;
  }

  /**
   * Returns root branch nodes or child action/file nodes for an expanded branch.
   *
   * @param element The parent sidebar node, or `undefined` for root items.
   * @returns Child `SidebarNode` items.
   */
  public getChildren(element?: SidebarNode): SidebarNode[] {
    if (!this.snapshot) {
      return [];
    }

    if (!element) {
      return this.snapshot.branchReport.branch_details.map((b) =>
        this.buildBranchItem(b)
      );
    }

    if (element.contextValue === "branchItem" && element.branchName) {
      const detail = this.snapshot.branchReport.branch_details.find(
        (b) => b.name === element.branchName
      );

      if (!detail) {
        return [];
      }

      const children: SidebarNode[] = [];

      children.push(
        new SidebarNode(
          `Switch to ${detail.name}`,
          vscode.TreeItemCollapsibleState.None,
          {
            description: detail.is_editor_branch ? "active" : "",
            icon: detail.is_editor_branch ? "check" : "source-control",
            branchName: detail.name,
            command: {
              command: "tes.switchEditorBranch",
              title: "Switch Editor Branch",
              arguments: [detail.name],
            },
          }
        )
      );

      children.push(
        new SidebarNode(
          `localhost:${detail.port}`,
          vscode.TreeItemCollapsibleState.None,
          {
            description: "dev server",
            icon: "globe",
            branchName: detail.name,
            port: detail.port,
            command: {
              command: "tes.openBranchPort",
              title: "Open Dev Server",
              arguments: [detail.port],
            },
          }
        )
      );

      for (const file of detail.modified_files) {
        children.push(
          new SidebarNode(file, vscode.TreeItemCollapsibleState.None, {
            description: "M",
            icon: "diff",
            contextValue: "branchFileItem",
            branchName: detail.name,
            filePath: file,
            command: {
              command: "tes.diffBranchFile",
              title: "Diff with main",
              arguments: [detail.name, file],
            },
          })
        );
      }

      return children;
    }

    return [];
  }

  /**
   * Builds a top-level collapsible `SidebarNode` for a single branch.
   *
   * @param detail The branch status metadata.
   * @returns A configured `SidebarNode` representing the branch.
   */
  private buildBranchItem(detail: BranchDetail): SidebarNode {
    const badges: string[] = [detail.short_commit || "HEAD"];

    if (detail.is_editor_branch) {
      badges.push("active");
    }

    badges.push(`:${detail.port}`);

    if (detail.modified_files.length > 0) {
      badges.push(`${detail.modified_files.length}M`);
    }

    return new SidebarNode(
      detail.name,
      vscode.TreeItemCollapsibleState.Collapsed,
      {
        description: badges.join(" · "),
        tooltip: `${detail.name} (${detail.short_commit}) · :${detail.port}`,
        contextValue: "branchItem",
        icon: detail.is_editor_branch ? "check" : "source-control",
        branchName: detail.name,
        port: detail.port,
      }
    );
  }
}

/**
 * TreeDataProvider for the "Sessions" view.
 */
export class SessionsTreeProvider
  implements vscode.TreeDataProvider<SidebarNode>
{
  private snapshot: WorkspaceStateSnapshot | undefined;
  private readonly onDidChangeTreeDataEmitter = new vscode.EventEmitter<
    SidebarNode | undefined | void
  >();
  public readonly onDidChangeTreeData = this.onDidChangeTreeDataEmitter.event;

  /**
   * Updates the snapshot and triggers a TreeView refresh.
   *
   * @param snapshot The latest repository state snapshot.
   */
  public setSnapshot(snapshot: WorkspaceStateSnapshot | undefined): void {
    this.snapshot = snapshot;
    this.onDidChangeTreeDataEmitter.fire();
  }

  /**
   * Returns the `TreeItem` representation for `element`.
   *
   * @param element The sidebar node to render.
   * @returns The corresponding VS Code `TreeItem`.
   */
  public getTreeItem(element: SidebarNode): vscode.TreeItem {
    return element;
  }

  /**
   * Returns active session nodes or child symbol/file nodes for an expanded session.
   *
   * @param element The parent sidebar node, or `undefined` for root items.
   * @returns Child `SidebarNode` items.
   */
  public getChildren(element?: SidebarNode): SidebarNode[] {
    if (!this.snapshot) {
      return [];
    }

    const activeAgents = this.snapshot.radarReport.active_agents ?? [];

    if (!element) {
      const agents = activeAgents.filter((a) => a.agent_id !== "editor");

      if (agents.length === 0) {
        return [
          new SidebarNode(
            "No active sessions",
            vscode.TreeItemCollapsibleState.None,
            {
              icon: "info",
            }
          ),
        ];
      }

      return agents.map((a) => this.buildSessionNode(a));
    }

    if (element.contextValue === "sessionItem" && element.sessionId) {
      const agent = activeAgents.find((a) => a.agent_id === element.sessionId);

      if (!agent) {
        return [];
      }

      const branch =
        agent.target_branch ?? this.snapshot.branchReport.editor_branch;
      const wp = agent.waypoint;
      const isIdeGui =
        wp?.ide_pid !== undefined && wp?.ide_pid !== null && !wp?.tty;
      const canJumpToTerminal =
        !isIdeGui &&
        Boolean(
          wp?.tty || wp?.term_session_id || agent.agent_id.startsWith("term-")
        );

      const children: SidebarNode[] = [];

      if (canJumpToTerminal) {
        children.push(
          new SidebarNode(
            "Jump to Terminal",
            vscode.TreeItemCollapsibleState.None,
            {
              description: wp?.term_program ?? wp?.tty ?? "focus",
              icon: "terminal",
              sessionId: agent.agent_id,
              command: {
                command: "tes.focusSession",
                title: "Jump to Terminal",
                arguments: [agent.agent_id],
              },
            }
          )
        );
      }

      children.push(
        new SidebarNode(
          `Switch to ${branch}`,
          vscode.TreeItemCollapsibleState.None,
          {
            icon: "source-control",
            branchName: branch,
            command: {
              command: "tes.switchEditorBranch",
              title: "Switch Editor Branch",
              arguments: [branch],
            },
          }
        )
      );

      for (const sym of agent.claimed_symbols ?? []) {
        children.push(
          new SidebarNode(sym, vscode.TreeItemCollapsibleState.None, {
            description: "claimed",
            icon: "symbol-method",
          })
        );
      }

      const modFiles =
        agent.modified_files ?? Object.keys(agent.own_blobs ?? {});

      for (const modFile of modFiles) {
        children.push(
          new SidebarNode(modFile, vscode.TreeItemCollapsibleState.None, {
            description: "M",
            icon: "edit",
            branchName: branch,
            filePath: modFile,
            command: {
              command: "tes.diffBranchFile",
              title: "Diff with main",
              arguments: [branch, modFile],
            },
          })
        );
      }

      return children;
    }

    return [];
  }

  /**
   * Builds a collapsible `SidebarNode` representing a single active agent or terminal session.
   *
   * @param agent The active session entry from the radar report.
   * @returns A configured `SidebarNode` for the session.
   */
  private buildSessionNode(agent: AgentEntry): SidebarNode {
    const isTerminal =
      agent.agent_id.startsWith("term-") ||
      agent.agent_id.startsWith("session-");
    const branch =
      agent.target_branch ??
      this.snapshot?.branchReport.editor_branch ??
      "main";
    const wp = agent.waypoint;
    const wpDetails: string[] = [];

    if (wp?.harness) {
      wpDetails.push(`harness: ${wp.harness}`);
    }

    if (wp?.term_program) {
      wpDetails.push(`terminal: ${wp.term_program}`);
    }

    if (wp?.tty) {
      wpDetails.push(`tty: ${wp.tty}`);
    }

    if (wp?.shell_pid) {
      wpDetails.push(`pid: ${wp.shell_pid}`);
    }

    const wpSuffix = wpDetails.length > 0 ? `\n${wpDetails.join(" · ")}` : "";

    return new SidebarNode(
      agent.agent_id,
      vscode.TreeItemCollapsibleState.Collapsed,
      {
        description: `${branch} · ${agent.intent}`,
        tooltip: `${agent.agent_id} (${branch})\n${agent.intent}${wpSuffix}`,
        contextValue: "sessionItem",
        icon: isTerminal ? "terminal" : "hubot",
        branchName: branch,
        sessionId: agent.agent_id,
      }
    );
  }
}

/**
 * TreeDataProvider for the "Conflicts & Checkpoints" view.
 */
export class ConflictsAndCheckpointsTreeProvider
  implements vscode.TreeDataProvider<SidebarNode>
{
  private snapshot: WorkspaceStateSnapshot | undefined;
  private readonly onDidChangeTreeDataEmitter = new vscode.EventEmitter<
    SidebarNode | undefined | void
  >();
  public readonly onDidChangeTreeData = this.onDidChangeTreeDataEmitter.event;

  /**
   * Updates the snapshot and triggers a TreeView refresh.
   *
   * @param snapshot The latest repository state snapshot.
   */
  public setSnapshot(snapshot: WorkspaceStateSnapshot | undefined): void {
    this.snapshot = snapshot;
    this.onDidChangeTreeDataEmitter.fire();
  }

  /**
   * Returns the `TreeItem` representation for `element`.
   *
   * @param element The sidebar node to render.
   * @returns The corresponding VS Code `TreeItem`.
   */
  public getTreeItem(element: SidebarNode): vscode.TreeItem {
    return element;
  }

  /**
   * Returns active conflicts, named checkpoints, and stashes (or stashed files when expanded).
   *
   * @param element The parent sidebar node, or `undefined` for root items.
   * @returns Child `SidebarNode` items.
   */
  public getChildren(element?: SidebarNode): SidebarNode[] {
    if (!this.snapshot) {
      return [];
    }

    if (element) {
      if (
        element.contextValue === "stashItem" &&
        element.stashIndex !== undefined
      ) {
        const stash = this.snapshot.stashes[element.stashIndex];

        if (!stash) {
          return [];
        }

        return Object.keys(stash.own_blobs ?? {}).map(
          (file) =>
            new SidebarNode(file, vscode.TreeItemCollapsibleState.None, {
              description: "stashed",
              icon: "file-code",
              branchName: stash.branch,
              filePath: file,
            })
        );
      }

      return [];
    }

    const items: SidebarNode[] = [];

    for (const c of this.snapshot.conflicts) {
      items.push(
        new SidebarNode(c.path, vscode.TreeItemCollapsibleState.None, {
          description: c.symbol ?? c.kind,
          tooltip: `Resolve conflict in ${c.path}`,
          contextValue: "conflictItem",
          icon: "warning",
          conflict: c,
          filePath: c.path,
          command: {
            command: "tes.openConflictStudio",
            title: "Open Conflict Resolver",
            arguments: [c.path],
          },
        })
      );
    }

    for (const cp of this.snapshot.checkpoints) {
      items.push(
        new SidebarNode(cp.label, vscode.TreeItemCollapsibleState.None, {
          description: cp.opId.slice(0, 8),
          tooltip: `Restore checkpoint '${cp.label}'`,
          contextValue: "checkpointItem",
          icon: "bookmark",
          checkpointLabel: cp.label,
          command: {
            command: "tes.restoreBranch",
            title: "Restore Checkpoint",
            arguments: [cp.label],
          },
        })
      );
    }

    this.snapshot.stashes.forEach((s, idx) => {
      const fileCount = Object.keys(s.own_blobs ?? {}).length;

      items.push(
        new SidebarNode(
          `stash@{${idx}}: ${s.message}`,
          fileCount > 0
            ? vscode.TreeItemCollapsibleState.Collapsed
            : vscode.TreeItemCollapsibleState.None,
          {
            description: `on ${s.branch} · ${fileCount} file(s)`,
            tooltip: `stash@{${idx}} (${s.id}) on ${s.branch}: ${s.message}`,
            contextValue: "stashItem",
            icon: "archive",
            branchName: s.branch,
            stashIndex: idx,
          }
        )
      );
    });

    if (items.length === 0) {
      items.push(
        new SidebarNode("No conflicts", vscode.TreeItemCollapsibleState.None, {
          icon: "check",
        })
      );
    }

    return items;
  }
}
