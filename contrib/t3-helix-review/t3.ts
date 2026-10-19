// T3's supported HTTP MCP transport. No database or provider-session internals.
import { realpath, readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { isAbsolute, join } from "node:path";

export type T3RunStatus = "preparing" | "queued" | "starting" | "running" | "waiting" | "completed" | "interrupted" | "failed" | "cancelled" | "rolled_back";
export type T3ThreadStatus = "idle" | T3RunStatus;
export interface T3Run { runId: string; status: T3RunStatus; ordinal: number; requestedAt: string; startedAt: string | null; completedAt: string | null }
export interface T3ThreadInfo {
  threadId: string;
  projectId: string;
  title: string;
  status: T3ThreadStatus;
  activeRunId: string | null;
  latestRunId: string | null;
  branch: string | null;
  worktreePath: string | null;
  /** Canonical local path of the actual thread checkout. */
  cwd: string;
  archived: boolean;
  settled: boolean;
  snoozed: boolean;
  recentRuns: T3Run[];
  linkedPullRequest?: { url: string; repository: string; number: number };
}
export interface T3Project { id: string; title: string; workspaceRoot: string; deletedAt?: string | null }
export interface T3ThreadListItem { threadId: string; title: string; status: T3ThreadStatus; latestRunId: string | null; settled: boolean; snoozed: boolean }
export interface T3ThreadList { projectId: string; currentThreadId: string | null; threads: T3ThreadListItem[]; nextCursor: number | null; total: number }
export interface T3SendResult { threadId: string; messageId: string; runId: string; status: T3RunStatus; delivery: "started" | "queued" | "steered" | "restarted"; taskId?: string }
export interface T3Connection { endpoint: string; authorization: string }
export interface T3ClientOptions extends T3Connection { timeoutMs?: number; fetch?: typeof fetch }

export class T3Error extends Error {
  readonly code: string;
  constructor(code: string, message: string) { super(message); this.name = "T3Error"; this.code = code; }
}

const PROTOCOL = "2025-06-18";
const MAX_RESPONSE_BYTES = 4 * 1024 * 1024;
const RUN_STATUSES = new Set(["preparing", "queued", "starting", "running", "waiting", "completed", "interrupted", "failed", "cancelled", "rolled_back"]);
type RecordValue = Record<string, unknown>;
function record(value: unknown): value is RecordValue { return typeof value === "object" && value !== null && !Array.isArray(value); }
function nonempty(value: unknown): value is string { return typeof value === "string" && value.trim().length > 0; }
function nullableString(value: unknown): value is string | null { return value === null || nonempty(value); }
function malformed(): never { throw new T3Error("invalid_response", "T3 returned an unsupported response. Update T3 Code or the Helix review adapter."); }
function requireString(value: unknown): string { if (!nonempty(value)) malformed(); return value; }
export function pullRequestUrl(value: string): string {
  const url = new URL(value);
  if (url.protocol !== "https:" || url.username || url.password || url.search || url.hash ||
      !/^\/[^/]+\/[^/]+\/pull\/[1-9]\d*$/u.test(url.pathname)) {
    throw new T3Error("invalid_request", "Use a full GitHub pull request URL.");
  }
  return url.href;
}
function assertProject(value: unknown): asserts value is T3Project {
  if (!record(value) || !nonempty(value.id) || !nonempty(value.title) || !nonempty(value.workspaceRoot)) malformed();
}

/** Only explicitly supplied environment credentials are used; nothing scans secret files. */
export function connectionFromEnvironment(env: NodeJS.ProcessEnv = process.env): T3Connection | undefined {
  const pairs = [
    [env.HX_T3_MCP_URL, env.HX_T3_MCP_TOKEN],
    [env.T3_MCP_URL, env.T3_MCP_BEARER_TOKEN],
    [env.T3_ACP_MCP_ENDPOINT, env.T3_ACP_MCP_AUTHORIZATION],
  ];
  for (const [endpoint, authorization] of pairs) {
    if (endpoint || authorization) {
      if (!endpoint || !authorization) throw new T3Error("configuration", "Both the T3 MCP endpoint and token must be configured.");
      return { endpoint, authorization };
    }
  }
  return undefined;
}

export function defaultT3BaseDir(env: NodeJS.ProcessEnv = process.env): string { return env.T3CODE_HOME || join(homedir(), ".t3"); }

/** This non-secret runtime descriptor is published and consumed by T3's own CLI. */
export async function discoverLocalEndpoint(baseDir = defaultT3BaseDir(), runtimeStatePath?: string): Promise<string> {
  let value: unknown;
  try { value = JSON.parse(await readFile(runtimeStatePath ?? join(baseDir, "userdata", "server-runtime.json"), "utf8")); }
  catch { throw new T3Error("server_unavailable", "No running T3 Code server was found. Open T3 Code before setting up Helix review."); }
  if (!record(value) || value.version !== 1 || !nonempty(value.origin)) malformed();
  const origin = validateEndpoint(value.origin);
  return new URL("/mcp", origin).href;
}

/** Verify the selected destination before sending it a stored credential. */
export async function readEnvironment(endpoint: string, options: { fetch?: typeof fetch; timeoutMs?: number } = {}): Promise<{ environmentId: string }> {
  const url = validateEndpoint(endpoint);
  let response: Response;
  try {
    response = await (options.fetch ?? fetch)(new URL("/.well-known/t3/environment", url), { headers: { accept: "application/json" }, redirect: "error", signal: AbortSignal.timeout(options.timeoutMs ?? 15_000) });
  } catch { throw new T3Error("server_unavailable", "Could not verify the selected T3 environment."); }
  if (!response.ok) { await response.body?.cancel(); throw new T3Error("server_unavailable", `T3 environment identity returned HTTP ${response.status}.`); }
  let value: unknown;
  try { value = await response.json(); } catch { malformed(); }
  if (!record(value) || !nonempty(value.environmentId)) malformed();
  return { environmentId: value.environmentId };
}

export function validateEndpoint(endpoint: string): URL {
  let url: URL;
  try { url = new URL(endpoint); } catch { throw new T3Error("configuration", "The T3 MCP endpoint must be an absolute HTTP or HTTPS URL."); }
  if (!["http:", "https:"].includes(url.protocol) || url.username || url.password || url.search || url.hash) {
    throw new T3Error("configuration", "The T3 MCP endpoint must be an HTTP or HTTPS URL without credentials, query, or fragment.");
  }
  return url;
}

export class T3Client {
  private readonly endpoint: string;
  private readonly authorization: string;
  private readonly timeoutMs: number;
  private readonly fetchImplementation: typeof fetch;
  private sessionId?: string;
  private protocolVersion = PROTOCOL;
  private initialized?: Promise<void>;
  private nextId = 1;

  constructor(options: T3ClientOptions) {
    this.endpoint = validateEndpoint(options.endpoint).href;
    if (!nonempty(options.authorization) || /[\r\n]/u.test(options.authorization)) throw new T3Error("configuration", "A valid T3 MCP token is required.");
    this.authorization = options.authorization.startsWith("Bearer ") ? options.authorization : `Bearer ${options.authorization}`;
    this.timeoutMs = options.timeoutMs ?? 15_000;
    if (!Number.isFinite(this.timeoutMs) || this.timeoutMs <= 0) throw new T3Error("configuration", "T3 timeout must be a positive number.");
    this.fetchImplementation = options.fetch ?? fetch;
  }

  private safeError(code: string, message: string): T3Error {
    // Server errors may echo a submitted header. Never include our credential.
    const token = this.authorization.slice(7);
    return new T3Error(code, message.replaceAll(this.authorization, "[redacted]").replaceAll(token, "[redacted]").slice(0, 500));
  }

  private async request(method: string, params?: unknown, notification = false): Promise<unknown> {
    const id = notification ? undefined : this.nextId++;
    const headers: Record<string, string> = { accept: "application/json, text/event-stream", "content-type": "application/json", authorization: this.authorization, "mcp-protocol-version": this.protocolVersion };
    if (this.sessionId) headers["mcp-session-id"] = this.sessionId;
    let response: Response;
    try {
      response = await this.fetchImplementation(this.endpoint, { method: "POST", headers, body: JSON.stringify({ jsonrpc: "2.0", ...(id === undefined ? {} : { id }), method, ...(params === undefined ? {} : { params }) }), redirect: "error", signal: AbortSignal.timeout(this.timeoutMs) });
    } catch { throw new T3Error("transport", "T3 MCP could not be reached or timed out. A send may have been accepted; retry with the same clientRequestId."); }
    if (!response.ok) {
      await response.body?.cancel();
      if (response.status === 404) { this.sessionId = undefined; this.initialized = undefined; }
      throw new T3Error(response.status === 401 || response.status === 403 ? "authentication" : "transport", `T3 MCP returned HTTP ${response.status}.`);
    }
    this.sessionId = response.headers.get("mcp-session-id") ?? this.sessionId;
    if (notification) { await response.body?.cancel(); return undefined; }
    let envelope: unknown;
    try { envelope = await readEnvelope(response, id!); }
    catch (error) { if (error instanceof T3Error) throw error; throw new T3Error("transport", "T3 MCP response ended or timed out before returning a result. Retry sends with the same clientRequestId."); }
    if (!record(envelope)) malformed();
    if (record(envelope.error)) throw this.safeError("rpc_error", typeof envelope.error.message === "string" ? envelope.error.message : "T3 MCP rejected the request.");
    if (!("result" in envelope)) malformed();
    return envelope.result;
  }

  private async connect(): Promise<void> {
    if (!this.initialized) {
      const attempt = (async () => {
        const result = await this.request("initialize", { protocolVersion: PROTOCOL, capabilities: {}, clientInfo: { name: "helix-review", version: "1.0.0" } });
        if (!record(result) || !nonempty(result.protocolVersion)) malformed();
        this.protocolVersion = result.protocolVersion;
        await this.request("notifications/initialized", undefined, true);
      })();
      this.initialized = attempt;
      try { await attempt; } catch (error) { if (this.initialized === attempt) this.initialized = undefined; throw error; }
    } else await this.initialized;
  }

  private async callTool(name: string, args: RecordValue): Promise<unknown> {
    await this.connect();
    const result = await this.request("tools/call", { name, arguments: args });
    if (!record(result)) malformed();
    let output = result.structuredContent;
    if (output === undefined && Array.isArray(result.content)) {
      for (const block of result.content) {
        if (record(block) && block.type === "text" && typeof block.text === "string") {
          try { output = JSON.parse(block.text); break; } catch { /* Only structured JSON is accepted. */ }
        }
      }
    }
    if (result.isError === true || (record(output) && output._tag === "OrchestratorMcpFailure")) {
      const code = record(output) && typeof output.code === "string" ? output.code : "tool_error";
      const message = record(output) && typeof output.message === "string" ? output.message : `T3 ${name} failed.`;
      throw this.safeError(code, message);
    }
    if (!record(output)) malformed();
    return output;
  }

  async readProject(projectId: string): Promise<T3Project> {
    const output = await this.callTool("t3_project_read", { projectId: requireString(projectId) });
    assertProject(output);
    if (output.id !== projectId || output.deletedAt) throw new T3Error("project_not_found", "The T3 project is no longer available.");
    return output;
  }

  async listProjects(): Promise<T3Project[]> {
    const projects: T3Project[] = [];
    let cursor: number | undefined;
    const seen = new Set<number>();
    do {
      const output = await this.callTool("t3_project_list", { limit: 100, ...(cursor === undefined ? {} : { cursor }) });
      if (!record(output) || !Array.isArray(output.projects) || !(output.nextCursor === null || Number.isSafeInteger(output.nextCursor))) malformed();
      for (const project of output.projects) { assertProject(project); if (!project.deletedAt) projects.push(project); }
      if (output.nextCursor === null) break;
      cursor = output.nextCursor as number;
      if (cursor < 0 || seen.has(cursor)) malformed();
      seen.add(cursor);
      if (seen.size > 100) throw new T3Error("invalid_response", "T3 project listing exceeded the page limit.");
    } while (true);
    return projects;
  }

  async listThreads(options: { projectId?: string; cursor?: number; limit?: number; includeSubagents?: boolean } = {}): Promise<T3ThreadList> {
    const output = await this.callTool("t3_thread_list", { limit: 100, ...options });
    if (!record(output) || !nonempty(output.projectId) || !nullableString(output.currentThreadId) || !Array.isArray(output.threads) || !(output.nextCursor === null || Number.isSafeInteger(output.nextCursor)) || !Number.isSafeInteger(output.total)) malformed();
    for (const thread of output.threads) {
      if (!record(thread) || !nonempty(thread.threadId) || typeof thread.title !== "string" || !(thread.status === "idle" || RUN_STATUSES.has(String(thread.status)))) malformed();
    }
    return output as unknown as T3ThreadList;
  }

  async readThread(threadId: string): Promise<T3ThreadInfo> {
    // Skip the timeline: polling must not acknowledge a child's terminal result.
    const output = await this.callTool("t3_thread_read", { threadId: requireString(threadId), afterPosition: Number.MAX_SAFE_INTEGER, limit: 1, runLimit: 10, maxCharsPerItem: 1 });
    if (!record(output) || !record(output.thread) || !Array.isArray(output.recentRuns)) malformed();
    const thread = output.thread;
    if (thread.threadId !== threadId || !nonempty(thread.projectId) || typeof thread.title !== "string" || !(thread.status === "idle" || RUN_STATUSES.has(String(thread.status))) || !nullableString(thread.activeRunId) || !nullableString(thread.latestRunId) || !nullableString(thread.branch) || !nullableString(thread.worktreePath) || typeof thread.archived !== "boolean" || typeof thread.settled !== "boolean" || typeof thread.snoozed !== "boolean") malformed();
    const workspace = thread.worktreePath ?? (await this.readProject(thread.projectId)).workspaceRoot;
    if (typeof workspace !== "string" || !isAbsolute(workspace)) throw new T3Error("workspace_unavailable", "T3 did not return an absolute local checkout path.");
    let cwd: string;
    try { cwd = await realpath(workspace); } catch { throw new T3Error("workspace_unavailable", "The T3 thread checkout is no longer available on this machine."); }
    for (const run of output.recentRuns) { if (!record(run) || !nonempty(run.runId) || !RUN_STATUSES.has(String(run.status))) malformed(); }
    const linked = thread.linkedPullRequest;
    const linkedPullRequest = record(linked) && nonempty(linked.url) && nonempty(linked.repository) &&
      typeof linked.number === "number" && Number.isSafeInteger(linked.number) && linked.number > 0 &&
      (() => { try { return new URL(pullRequestUrl(linked.url)).pathname === `/${linked.repository}/pull/${linked.number}`; } catch { return false; } })()
      ? { url: linked.url, repository: linked.repository, number: linked.number } : undefined;
    return { threadId, projectId: thread.projectId, title: thread.title, status: thread.status as T3ThreadStatus, activeRunId: thread.activeRunId, latestRunId: thread.latestRunId, branch: thread.branch, worktreePath: thread.worktreePath, cwd, archived: thread.archived, settled: thread.settled, snoozed: thread.snoozed, recentRuns: output.recentRuns as T3Run[], ...(linkedPullRequest ? { linkedPullRequest } : {}) };
  }

  /** URLs of every PR linked to the thread; T3 with multiple links leaves `linkedPullRequest` null. */
  async linkedPullRequests(threadId: string): Promise<string[]> {
    const output = await this.callTool("list_thread_pull_requests", { threadId: requireString(threadId) });
    if (!Array.isArray(output.pullRequests)) malformed();
    return output.pullRequests.map(pr => record(pr) && nonempty(pr.url) ? pr.url : malformed());
  }

  async send(threadId: string, message: string, clientRequestId: string, mode: "auto" | "queue" = "auto"): Promise<T3SendResult> {
    if (!nonempty(threadId) || !nonempty(message) || message.length > 120_000 || !nonempty(clientRequestId) || clientRequestId.length > 256) throw new T3Error("invalid_request", "Sending a review requires a thread, message (at most 120,000 characters), and stable clientRequestId (at most 256 characters).");
    if (mode !== "auto" && mode !== "queue") throw new T3Error("invalid_request", "Review delivery mode must be auto or queue.");
    // Auto lets T3 choose atomically; queue reserves a separate follow-up turn.
    const output = await this.callTool("t3_thread_send", { threadId, message, mode, clientRequestId });
    if (!record(output) || output.threadId !== threadId || !nonempty(output.messageId) || !nonempty(output.runId) || !RUN_STATUSES.has(String(output.status)) || !["started", "queued", "steered", "restarted"].includes(String(output.delivery))) malformed();
    return output as unknown as T3SendResult;
  }
}

async function readEnvelope(response: Response, id: number): Promise<unknown> {
  if (!response.body) malformed();
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  const isSse = response.headers.get("content-type")?.includes("text/event-stream");
  let buffer = "";
  let bytes = 0;
  const parse = (body: string): unknown => {
    let value: unknown;
    try { value = JSON.parse(body); } catch { malformed(); }
    if (record(value) && value.jsonrpc === "2.0" && value.id === id) return value;
    return undefined;
  };
  try {
    while (true) {
      const { value, done } = await reader.read();
      bytes += value?.byteLength ?? 0;
      if (bytes > MAX_RESPONSE_BYTES) throw new T3Error("invalid_response", "T3 MCP response exceeded the size limit.");
      buffer += decoder.decode(value, { stream: !done });
      if (isSse) {
        buffer = buffer.replace(/\r\n/gu, "\n");
        let boundary: number;
        while ((boundary = buffer.indexOf("\n\n")) >= 0) {
          const event = buffer.slice(0, boundary);
          buffer = buffer.slice(boundary + 2);
          const data = event.split("\n").filter((line) => line.startsWith("data:")).map((line) => line.slice(5).replace(/^ /u, "")).join("\n");
          if (!data) continue;
          const envelope = parse(data);
          if (envelope !== undefined) return envelope;
        }
      }
      if (done) break;
    }
    if (!isSse) { const envelope = parse(buffer); if (envelope !== undefined) return envelope; }
    malformed();
  } finally { await reader.cancel().catch(() => {}); reader.releaseLock(); }
}
