/**
 * Virtual document content provider for inspecting and diffing files across isolated branches.
 */

import * as vscode from "vscode";
import { CliClient } from "./client";

/**
 * Virtual document provider for `branch-view://<branch>/<path>` URIs so users can
 * view or side-by-side diff files from any branch or AI agent session without
 * switching the primary working tree if desired.
 */
export class BranchContentProvider implements vscode.TextDocumentContentProvider {
  public static readonly scheme = "branch-view";
  private readonly client: CliClient;
  private readonly onDidChangeEmitter = new vscode.EventEmitter<vscode.Uri>();
  public readonly onDidChange = this.onDidChangeEmitter.event;

  /**
   * Creates a virtual document content provider backed by `client`.
   *
   * @param client The CLI client used to read file contents from target branches.
   */
  public constructor(client: CliClient) {
    this.client = client;
  }

  /**
   * Builds a `branch-view:` URI for `branch` and repository-relative `relPath`.
   *
   * @param branch The target branch or revision name.
   * @param relPath The repository-relative file path.
   * @returns A formatted VS Code URI for the virtual document.
   */
  public static buildUri(branch: string, relPath: string): vscode.Uri {
    const baseRel = relPath.split("::")[0]?.trim() || relPath;
    const cleanRel = baseRel.replace(/^\/+/, "");

    return vscode.Uri.from({
      scheme: BranchContentProvider.scheme,
      path: `/${cleanRel}`,
      query: branch,
    });
  }

  /**
   * Resolves the text content of a `branch-view:` URI by reading the file from the target branch.
   *
   * @param uri The virtual document URI to resolve.
   * @returns The UTF-8 file contents, or an empty string if the file does not exist on that branch.
   */
  public async provideTextDocumentContent(uri: vscode.Uri): Promise<string> {
    const branch = uri.query || decodeURIComponent(uri.authority);
    const rawRel = uri.path.replace(/^\/+/, "");
    const relPath = rawRel.split("::")[0]?.trim() || rawRel;

    try {
      return await this.client.readBranchFile(branch, relPath);
    } catch {
      return "";
    }
  }
}
