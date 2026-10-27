import { execFile } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { connect } from "node:net";
import { mkdir, readFile, readdir, realpath, rename, rm, rmdir, stat, writeFile } from "node:fs/promises";
import { dirname, join, relative, resolve, sep } from "node:path";
import { homedir, tmpdir } from "node:os";
import { setTimeout as delay } from "node:timers/promises";
import { EmptyReviewError, ReviewBatch, buildHandoff } from "../helix-review/index.ts";
import type { T3ThreadInfo, T3SendResult } from "./t3.ts";
import { T3Error, pullRequestUrl } from "./t3.ts";

export interface ReviewClient {
  readThread(id: string): Promise<T3ThreadInfo>;
  linkedPullRequests(id: string): Promise<string[]>;
  send(id: string, message: string, clientRequestId: string, mode?: "auto" | "queue"): Promise<T3SendResult>;
}
export type ReviewMode = "work" | "review";
export interface BridgeOptions {
  client: ReviewClient;
  endpoint: string;
  threadId: string;
  stateRoot?: string;
  socketPath?: string;
  baseline?: string;
  pullRequest?: string;
  mode?: ReviewMode;
  /**
   * An attached bridge is restarted by T3, which cannot pass --base, so a saved baseline that a
   * rewritten HEAD (amend, rebase, force-push) dropped from the branch is replaced by HEAD.
   */
  attach?: boolean;
  /** How long, in ms, to wait for a live bridge holding this thread's lock to exit. */
  lockWait?: number;
  /** Once aborted, the bridge sends nothing more to the editor. */
  signal?: AbortSignal;
  onStatus?: (message: string) => void;
}
interface State {
  version: 1;
  threadId: string;
  cwd: string;
  repo: string;
  branch: string;
  baseline: string;
  lastRun?: string | null;
  snapshots: Record<string, string>;
  pullRequest?: string;
  mode: ReviewMode;
  reviewHead?: string;
}
interface Delivery { id: string; message: string; indexTree?: string; head?: string; attempted?: boolean }
const active = new Set(["preparing", "queued", "starting", "running", "waiting"]);
export const isActive = (thread: T3ThreadInfo) => active.has(thread.status);
const digest = (value: string) => createHash("sha256").update(value).digest("hex");
const inHistory = (repo: string, commit: string) =>
  git(repo, ["merge-base", "--is-ancestor", commit, "HEAD"]).then(() => true, () => false);

export function git(cwd: string, args: string[], input?: string | Buffer): Promise<string> {
  return new Promise((done, fail) => {
    const env: NodeJS.ProcessEnv = { ...process.env, GIT_TERMINAL_PROMPT: "0" };
    for (const key of ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR"]) delete env[key];
    const child = execFile("git", ["--literal-pathspecs", "-c", "core.fsmonitor=", "-C", cwd, ...args],
      { env, encoding: "utf8", maxBuffer: 16 * 1024 * 1024, timeout: 15_000 },
      (error, stdout, stderr) => error ? fail(new Error(`Git ${args[0]} failed: ${stderr.trim() || error.message}`)) : done(stdout));
    child.stdin?.end(input);
  });
}

export async function writeJson(path: string, value: unknown): Promise<void> {
  const temp = `${path}.${randomUUID()}.tmp`;
  await writeFile(temp, JSON.stringify(value, null, 2) + "\n", { mode: 0o600, flag: "wx" });
  try { await rename(temp, path); } finally { await rm(temp, { force: true }); }
}

export function quoteCommand(value: string): string {
  if (/\p{Cc}/u.test(value)) throw new Error("HX command arguments cannot contain control characters.");
  return `'${value.replaceAll("'", "''")}'`;
}

export function sendCommands(path: string, commands: string[]): Promise<string[]> {
  return new Promise((done, fail) => {
    const socket = connect(path);
    let pending = "";
    const replies: string[] = [];
    let finished = false;
    const finish = (error?: Error) => {
      if (finished) return;
      finished = true;
      socket.destroy();
      error ? fail(error) : done(replies);
    };
    socket.setEncoding("utf8");
    socket.setTimeout(15_000, () => finish(new Error("HX did not acknowledge the review commands.")));
    socket.on("error", finish);
    socket.on("close", () => finish(new Error("HX closed before acknowledging the review commands.")));
    socket.on("connect", () => socket.write(commands.join("\n") + "\n"));
    socket.on("data", chunk => {
      const lines = (pending + chunk).split("\n");
      pending = lines.pop()!;
      for (const line of lines) {
        if (!/^ok(?: |$)/u.test(line)) { finish(new Error(`HX: ${line}`)); return; }
        replies.push(line);
      }
      if (replies.length === commands.length) finish();
    });
  });
}

/** Whether an editor accepts connections on `path`; a missing or refusing socket means it is gone. */
export function editorListening(path: string): Promise<boolean> {
  return new Promise(done => {
    const socket = connect(path);
    socket.once("connect", () => { socket.end(); done(true); });
    socket.once("error", error => done(!["ENOENT", "ECONNREFUSED"].includes((error as NodeJS.ErrnoException).code ?? "")));
  });
}

async function readJson<T>(path: string): Promise<T | undefined> {
  try { return JSON.parse(await readFile(path, "utf8")); }
  catch (error) { if ((error as NodeJS.ErrnoException).code === "ENOENT") return undefined; throw error; }
}

/**
 * Takes the bridge lock in `dir` for `token`, recovering it from a dead owner. While a live owner
 * holds it, or another bridge is between creating or removing it and its owner file, retries for
 * `wait` ms.
 */
async function acquireLock(dir: string, token: string, wait: number): Promise<void> {
  const path = join(dir, "bridge.lock");
  const deadline = Date.now() + wait;
  const retry = async () => {
    if (Date.now() >= deadline) return false;
    await delay(250);
    return true;
  };
  let recoveries = 0;
  while (recoveries < 8) {
    try { await mkdir(path, { mode: 0o700 }); }
    catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
      let owner: { pid: number; token: string } | undefined;
      try { owner = await readJson(join(path, "owner.json")); }
      catch (error) {
        if ((error as NodeJS.ErrnoException).code !== "ENOTDIR") throw error;
        throw new Error(`A legacy review lock exists at ${path}. Close other bridges, then remove that file before reopening.`);
      }
      if (!owner || !Number.isSafeInteger(owner.pid) || owner.pid <= 0 || !/^[0-9a-f-]{36}$/u.test(owner.token)) {
        if (await retry()) continue;
        throw new Error(`An incomplete review lock exists at ${path}. Close other bridges before removing that directory.`);
      }
      try { process.kill(owner.pid, 0); }
      catch (error) {
        if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error;
        // Keep each nonempty quarantine: a delayed contender must not rename a
        // newer owner's lock over the same dead owner's recovery destination.
        try { await rename(path, `${path}.stale-${owner.token}`); }
        catch (error) {
          if (!["ENOENT", "EEXIST", "ENOTEMPTY"].includes((error as NodeJS.ErrnoException).code ?? "")) throw error;
        }
        recoveries++;
        continue;
      }
      if (await retry()) continue;
      throw new Error("This thread already has an HX review open. Return to that editor or close it first.");
    }
    await writeFile(join(path, "owner.json"), JSON.stringify({ pid: process.pid, token }), { flag: "wx", mode: 0o600 });
    return;
  }
  throw new Error("The HX review lock kept changing. Close other bridges before reopening.");
}

async function releaseLock(dir: string, token: string): Promise<void> {
  const path = join(dir, "bridge.lock");
  const ownerPath = join(path, "owner.json");
  const owner = await readJson<{ token: string }>(ownerPath);
  if (owner?.token === token) { await rm(ownerPath); await rmdir(path); }
}

export class ReviewBridge {
  readonly client: ReviewClient;
  readonly dir: string;
  readonly outbox: string;
  readonly socketPath: string;
  readonly threadId: string;
  private state: State;
  private thread: T3ThreadInfo;
  private options: BridgeOptions;
  private socketIdentity?: string;
  private socketLabel?: string;
  private fingerprint?: string;
  private lastStatus?: string;
  private lockToken: string;
  private closed = false;

  private constructor(options: BridgeOptions, thread: T3ThreadInfo, dir: string, socketPath: string, state: State, lockToken: string) {
    this.options = options; this.client = options.client; this.thread = thread; this.threadId = thread.threadId;
    this.dir = dir; this.outbox = join(dir, "outbox"); this.socketPath = socketPath; this.state = state; this.lockToken = lockToken;
  }

  get repo(): string { return this.state.repo; }
  get baseline(): string { return this.state.baseline; }
  get pullRequest(): string | undefined { return this.state.pullRequest; }
  get mode(): ReviewMode { return this.state.mode; }
  get title(): string { return this.thread.title; }

  static async open(options: BridgeOptions): Promise<ReviewBridge> {
    const thread = await options.client.readThread(options.threadId);
    const mode = options.mode ?? (options.pullRequest ? "review" : "work");
    if (mode === "work" && options.pullRequest) throw new Error("Open Review a PR mode to attach a pull request.");
    if (thread.archived) throw new Error("This T3 thread is archived. Unarchive it, then open the file in T3 again.");
    const repo = await realpath((await git(thread.cwd, ["rev-parse", "--show-toplevel"])).trim());
    const branch = (await git(repo, ["symbolic-ref", "--quiet", "--short", "HEAD"]).catch(() => "")).trim();
    if (thread.branch !== null && thread.branch !== branch) {
      throw new Error(`This thread belongs to branch ${thread.branch}, but its checkout is on ${branch || "a detached HEAD"}. Restore the thread's branch before reviewing.`);
    }
    const id = digest(`${options.endpoint}\0${thread.threadId}\0${thread.cwd}${mode === "review" ? "\0review" : ""}`).slice(0, 24);
    const root = options.stateRoot ?? join(process.env.XDG_STATE_HOME || join(homedir(), ".local", "state"), "helix", "t3-review");
    const dir = join(root, id);
    await mkdir(dir, { recursive: true, mode: 0o700 });
    const lockToken = randomUUID();
    // Read the saved state only under the lock: a predecessor still shutting down may save it.
    await acquireLock(dir, lockToken, options.lockWait ?? 0);
    try {
      const previous = await readJson<State>(join(dir, "state.json"));
      if (previous && (previous.version !== 1 || previous.threadId !== thread.threadId || previous.cwd !== thread.cwd || previous.repo !== repo)) {
        throw new Error("The saved HX review binding does not match this thread and worktree.");
      }
      if (previous && previous.branch !== branch) throw new Error("This review belongs to a different branch. Restore that branch to resume its pending feedback.");
      const revision = options.baseline ?? previous?.baseline ?? "HEAD";
      let baseline = (await git(repo, ["rev-parse", "--verify", "--end-of-options", `${revision}^{commit}`])).trim();
      if (!await inHistory(repo, baseline)) {
        if (!options.attach || options.baseline !== undefined) {
          throw new Error("The review baseline is no longer in this branch. Reopen with --base <revision> to choose the review range.");
        }
        baseline = (await git(repo, ["rev-parse", "HEAD"])).trim();
      }
      const runtime = process.env.XDG_RUNTIME_DIR || join(tmpdir(), `helix-${process.getuid?.() ?? "user"}`);
      const socketPath = options.socketPath ?? join(runtime, "helix", `t3-${id}.sock`);
      if (Buffer.byteLength(socketPath) > 103) throw new Error("HX socket path is too long. Use --socket with a shorter private path.");
      let prUrl = mode === "review" ? options.pullRequest ?? thread.linkedPullRequest?.url ?? previous?.pullRequest : undefined;
      if (mode === "review" && !prUrl) {
        const linked = await options.client.linkedPullRequests(thread.threadId);
        if (linked.length > 1) throw new Error("This thread links several pull requests. Pass --pr <url> to choose one.");
        prUrl = linked[0];
      }
      if (mode === "review" && !prUrl) throw new Error("Link a pull request to this thread or pass --pr <url> before opening Review a PR.");
      const state: State = { version: 1, threadId: thread.threadId, cwd: thread.cwd, repo, branch, baseline, mode,
        ...(prUrl ? { pullRequest: pullRequestUrl(prUrl) } : {}),
        ...(mode === "review" ? { reviewHead: (await git(repo, ["rev-parse", "HEAD"])).trim() } : {}),
        snapshots: previous?.snapshots ?? {}, lastRun: previous?.lastRun };
      const bridge = new ReviewBridge(options, thread, dir, socketPath, state, lockToken);
      for (const name of ["outbox", "deliveries", "receipts"]) await mkdir(join(dir, name), { mode: 0o700, recursive: true });
      if (mode === "work" && !previous && !isActive(thread)) {
        await bridge.snapshot();
        bridge.state.lastRun = thread.latestRunId;
      }
      await bridge.save();
      return bridge;
    } catch (error) { await releaseLock(dir, lockToken); throw error; }
  }

  private save(): Promise<void> { return writeJson(join(this.dir, "state.json"), this.state); }

  private send(commands: string[]): Promise<string[]> {
    return this.options.signal?.aborted ? Promise.resolve([]) : sendCommands(this.socketPath, commands);
  }

  private async paths(): Promise<string[]> {
    const tracked = await git(this.repo, ["diff", "--name-only", "-z", this.baseline, "--"]);
    const untracked = await git(this.repo, ["ls-files", "--others", "--exclude-standard", "-z"]);
    const paths = [...new Set((tracked + untracked).split("\0").filter(Boolean))];
    if (paths.length > 1000) throw new Error("More than 1000 changed files; narrow the review baseline before opening HX.");
    return paths.map(path => join(this.repo, path));
  }

  private async inside(path: string): Promise<void> {
    let canonical: string;
    try { canonical = await realpath(path); }
    catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
      let parent = dirname(path);
      for (;;) {
        try { canonical = resolve(await realpath(parent), relative(parent, path)); break; }
        catch (error) {
          if ((error as NodeJS.ErrnoException).code !== "ENOENT" || dirname(parent) === parent) throw error;
          parent = dirname(parent);
        }
      }
    }
    const rel = relative(this.repo, canonical);
    if (!rel || rel === ".." || rel.startsWith(`..${sep}`) || resolve(this.repo, rel) !== canonical) throw new Error("Review contains a path outside the thread's worktree.");
  }

  private async snapshot(): Promise<void> {
    const snapshots: Record<string, string> = {};
    for (const path of await this.paths()) {
      await this.inside(path);
      let bytes: Buffer;
      try { if ((await stat(path)).size > 8 * 1024 * 1024) continue; bytes = await readFile(path); }
      catch (error) { if ((error as NodeJS.ErrnoException).code === "ENOENT") bytes = Buffer.alloc(0); else throw error; }
      if (bytes.includes(0)) continue;
      snapshots[path] = (await git(this.repo, ["hash-object", "-w", "--no-filters", "--stdin"], bytes)).trim();
    }
    this.state.snapshots = snapshots;
  }

  private async announce(message: string): Promise<void> {
    message = message.replace(/[\p{Cc}\u2028\u2029]/gu, " ").slice(0, 500);
    if (message === this.lastStatus) return;
    this.lastStatus = message;
    this.options.onStatus?.(message);
    await this.send([`echo ${quoteCommand(message)}`]).catch(() => undefined);
  }

  private async validateBinding(): Promise<T3ThreadInfo> {
    const thread = await this.client.readThread(this.threadId);
    if (thread.archived || thread.cwd !== this.state.cwd) throw new Error("This T3 thread was archived or moved to another worktree. Feedback is kept: unarchive the thread or restore its worktree, then open the file in T3 again.");
    const branch = (await git(this.repo, ["symbolic-ref", "--quiet", "--short", "HEAD"]).catch(() => "")).trim();
    if (branch !== this.state.branch || (thread.branch !== null && thread.branch !== branch)) throw new Error("The checkout no longer matches this thread's branch. Restore that branch before sending feedback.");
    return thread;
  }

  /** Delivers pending feedback and refreshes the editor; resolves to whether anything changed. */
  async tick(): Promise<boolean> {
    if (this.closed) return false;
    try {
      const thread = await this.validateBinding();
      const pending = (await readdir(this.outbox)).filter(name => name.endsWith(".json")).sort();
      for (const name of pending) {
        try { await this.deliver(name, true); }
        catch (error) { if (this.loginRejected(error)) throw error; await this.announce(`T3: feedback kept — ${(error as Error).message}`); }
      }
      const run = thread.latestRunId !== this.thread.latestRunId || thread.status !== this.thread.status;
      if (this.mode === "work" && !isActive(thread) && thread.latestRunId !== this.state.lastRun) {
        await this.snapshot(); this.state.lastRun = thread.latestRunId; await this.save();
        await this.announce(`T3: ${thread.title} — ${thread.status}; review updated`);
      }
      this.thread = thread;
      // Feedback goes to the thread even when the editor can't be refreshed.
      const refreshed = await this.refresh().catch(async (error: Error) => {
        await this.announce(`T3: ${error.message}`);
        return false;
      });
      let delivered = 0;
      for (const name of pending) {
        try { await this.deliver(name); delivered++; }
        catch (error) { if (this.loginRejected(error)) throw error; await this.announce(`T3: feedback kept — ${(error as Error).message}`); }
      }
      return delivered > 0 || run || refreshed;
    } catch (error) {
      if (this.loginRejected(error)) throw error;
      await this.announce(`T3: ${(error as Error).message}`);
      return false;
    }
  }

  /** An attached bridge cannot pair again; it ends, and T3's next open relaunches it with the current login. */
  private loginRejected(error: unknown): boolean {
    return this.options.attach === true && error instanceof T3Error && error.code === "authentication";
  }

  /** Binds a new or renamed editor and refreshes its review; resolves to whether it sent anything. */
  async refresh(): Promise<boolean> {
    const info = await stat(this.socketPath).catch(error => {
      this.socketIdentity = undefined;
      throw error;
    });
    const identity = `${info.ino}:${info.ctimeMs}`;
    const newSocket = identity !== this.socketIdentity;
    const title = [...`T3: ${this.title.replace(/[\p{Cc}\u2028\u2029]/gu, " ")}`].slice(0, 120).join("");
    const bound = newSocket || title !== this.socketLabel;
    if (bound) {
      await this.sendWithBaseline(() => [`review-handoff ${quoteCommand(this.outbox)} ${quoteCommand(title)}`,
        ...(this.mode === "work" ? [`review-baseline ${this.baseline}`] : [])]);
      if (newSocket && this.state.pullRequest) {
        await this.send([`review-pr-attach ${quoteCommand(this.state.pullRequest)}`]);
      }
      this.socketIdentity = identity;
      this.socketLabel = title;
      this.fingerprint = undefined;
      await this.announce(`${title} — connected; Space A a asks about your selection`);
    }
    if (this.mode === "review") return bound;
    const paths = await this.paths();
    const files = await Promise.all(paths.map(async path => {
      const value = await stat(path).catch(() => undefined);
      return `${path}:${value?.mtimeMs}:${value?.size}`;
    }));
    const fingerprint = digest((await git(this.repo, ["rev-parse", "HEAD"])) + files.join("\0") + await git(this.repo, ["diff", "--cached", "--raw", "--"]));
    if (fingerprint === this.fingerprint) return bound;
    await this.sendWithBaseline(() => [`review-baseline ${this.baseline}`]);
    this.fingerprint = fingerprint;
    return true;
  }

  /**
   * Sends `commands`, built from the current baseline. When HX refuses the baseline because a
   * rewritten HEAD (amend, rebase, force-push) dropped it from the branch, reviews from HEAD instead;
   * resending the stale one would refuse every refresh.
   */
  private async sendWithBaseline(commands: () => string[]): Promise<void> {
    try { await this.send(commands()); }
    catch (error) {
      if (!/not in this branch's history/u.test((error as Error).message) || await inHistory(this.repo, this.baseline)) throw error;
      this.state.baseline = (await git(this.repo, ["rev-parse", "HEAD"])).trim();
      await this.save();
      await this.announce(`T3: HEAD was rewritten; reviewing from ${this.baseline.slice(0, 8)}`);
      await this.send(commands());
    }
  }

  async deliver(name: string, prepareOnly = false): Promise<T3SendResult | undefined> {
    if (!/^[^/\\]+\.json$/u.test(name)) throw new Error("Invalid handoff filename.");
    const path = join(this.outbox, name);
    const batch = await ReviewBatch.read(path);
    if (batch.target && batch.target !== this.threadId && batch.target !== `t3-${this.threadId.slice(0, 8)}`) {
      throw new Error(`This feedback targets @${batch.target}; this editor is bound to ${this.title}.`);
    }
    for (const entry of [...batch.files, ...batch.comments]) await this.inside(entry.path);
    const thread = await this.validateBinding();
    const id = `hx-${digest(`${this.dir}\0${name}\0${await readFile(path, "utf8")}`)}`;
    const receiptPath = join(this.dir, "receipts", `${id}.json`);
    const receipt = await readJson<T3SendResult>(receiptPath);
    if (receipt) { if (!prepareOnly) await rm(path); return receipt; }
    const deliveryPath = join(this.dir, "deliveries", `${id}.json`);
    let delivery = await readJson<Delivery>(deliveryPath);
    if (!delivery) {
      if (this.mode === "review") {
        if (batch.request?.kind === "commit-staged") throw new Error("PR review drafts cannot request commits. Open Work together to change the checkout.");
        const message = `You are helping the user review ${this.pullRequest} at commit ${this.state.reviewHead}. Read the reviewed files from that commit with git show; the current checkout may contain unrelated work. Analyze the PR and the selection below as a co-reviewer. Selected text may contain a proposed edit held only in an HX suggestion buffer; it is not a saved checkout change. Return findings, explanations, and suggested replacement code in this conversation. Do not edit checkout files, stage, commit, push, switch branches, or publish to GitHub. The user will turn proposals into HX drafts and explicitly publish their review.\n\n${JSON.stringify({ request: batch.request, selections: batch.comments }, null, 2)}`;
        delivery = { id, message };
      } else {
        const review = batch.request?.kind === "pr-review";
        const pullRequests = review ? await this.client.linkedPullRequests(this.threadId) : [];
        if (review && !pullRequests.length) throw new Error("Link a pull request to this thread first");
        const states = await batch.inspect(this.repo);
        for (const [file, state] of states) {
          if (!state.problem) continue;
          try { await stat(file); }
          catch (error) { if ((error as NodeJS.ErrnoException).code === "ENOENT") continue; throw error; }
          throw new Error(`Cannot verify ${file}: ${state.problem}. Feedback is kept; no deletion has been inferred.`);
        }
        const { warnings } = batch.render(path, states);
        // Every file and the message use one HEAD: an agent commit meanwhile would mix commits.
        const head = review ? (await git(this.repo, ["rev-parse", "HEAD"])).trim() : undefined;
        const built = await buildHandoff(batch, { repo: this.repo, snapshots: this.state.snapshots, baseline: this.baseline, strictCurrentReads: true, head })
          .catch(error => { if (error instanceof EmptyReviewError) return error; throw error; });
        if (head && (await git(this.repo, ["rev-parse", "HEAD"])).trim() !== head) throw new Error("HEAD moved while the pull request review was prepared");
        if (built instanceof EmptyReviewError) {
          if (!prepareOnly) { await rm(path); await this.announce(`T3: nothing published — ${built.message}`); }
          return;
        }
        const handoff = built;
        let message: string;
        if (handoff) message = handoff.content;
        else if (batch.files.length) {
          const files = batch.files.map(file => ({ path: file.path, baseline: file.baseline }));
          message = `The user explicitly handed back these reviewed files from Helix. Re-read their saved contents and treat them as the user's intended version. No separate edit hunks could be distinguished from the bridge's observed snapshot; this does not mean the user made no corrections. Preserve the saved code and do not restore an earlier agent version. Report what remains to do.\n\n${JSON.stringify(files, null, 2)}`;
        } else {
          if (!prepareOnly) { await rm(path); await this.announce("T3: review received; no changes or instructions to send"); }
          return;
        }
        if (warnings.length) message += `\n\nReview freshness warnings (re-read before acting):\n${warnings.map(w => `- ${w}`).join("\n")}`;
        delivery = { id, message };
        if (batch.request?.kind === "commit-staged") {
          delivery.indexTree = (await git(this.repo, ["write-tree"])).trim();
          delivery.head = (await git(this.repo, ["rev-parse", "HEAD"])).trim();
          delivery.message += `\n\nBefore committing, verify HEAD is ${delivery.head} and git write-tree is ${delivery.indexTree}. If either differs, stop and ask the user to review the staged content again. Do not stage other changes, push, or change branches.`;
        }
        if (review) {
          const target = pullRequests.length === 1 ? pullRequests[0]
            : `the one of ${pullRequests.join(", ")} whose head branch is ${this.state.branch || "this checkout's HEAD"}`;
          delivery.message += `\n\nThe user wrote this review against the checkout's HEAD, ${head}. Publish the review on ${target} with T3's submit_pull_request_review MCP tool, instead of the host's tools: pass the pull request, headSha "${head}", event "comment", the review body, and comments as {path, line, startLine, side, body}, with path relative to the repository root, line the last and startLine the first line of a multi-line range, side "right" (head lines, the default) or "left" (removed base lines), and each suggestion's body a \`\`\`suggestion fence. If the tool reports that the pull request's head is not ${head}, stop and tell the user instead of posting anything. Only if that tool is unavailable, publish the same review with the host's CLI, and only when the pull request's head commit is ${head}.`;
        }
      }
      await writeJson(deliveryPath, delivery);
    }
    if (prepareOnly) return;
    if (delivery.indexTree && !delivery.attempted) {
      if (isActive(thread)) throw new Error("Commit request is waiting for the agent to finish; the staged snapshot is preserved.");
      if (delivery.indexTree !== (await git(this.repo, ["write-tree"])).trim() || delivery.head !== (await git(this.repo, ["rev-parse", "HEAD"])).trim()) {
        throw new Error(`The staged content changed. Close HX and run hx-t3 discard --batch ${name} to cancel this request, then review and request the commit again.`);
      }
    }
    delivery.attempted = true;
    await writeJson(deliveryPath, delivery);
    const result = await this.client.send(this.threadId, delivery.message, delivery.id, delivery.indexTree ? "queue" : "auto");
    await writeJson(receiptPath, result);
    await rm(path);
    await this.announce(`T3: ${result.delivery} feedback for ${this.title}; answer in the thread`);
    return result;
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    await releaseLock(this.dir, this.lockToken);
  }

  async pending(): Promise<string[]> {
    return (await readdir(this.outbox)).filter(name => name.endsWith(".json")).sort();
  }

  async discard(name: string): Promise<void> {
    if (!/^[^/\\]+\.json$/u.test(name)) throw new Error("Use an exact batch name from hx-t3 pending.");
    const archive = join(this.dir, "discarded");
    await mkdir(archive, { recursive: true, mode: 0o700 });
    await rename(join(this.outbox, name), join(archive, `${randomUUID()}-${name}`));
  }
}
