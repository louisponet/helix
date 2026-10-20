import { execFile } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { connect } from "node:net";
import { mkdir, readFile, readdir, realpath, rename, rm, rmdir, stat, writeFile } from "node:fs/promises";
import { dirname, join, relative, resolve, sep } from "node:path";
import { homedir, tmpdir } from "node:os";
import { ReviewBatch, buildHandoff } from "../helix-review/index.ts";
import type { T3ThreadInfo, T3SendResult } from "./t3.ts";
import { pullRequestUrl } from "./t3.ts";

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

async function readJson<T>(path: string): Promise<T | undefined> {
  try { return JSON.parse(await readFile(path, "utf8")); }
  catch (error) { if ((error as NodeJS.ErrnoException).code === "ENOENT") return undefined; throw error; }
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
  private lockToken = randomUUID();
  private closed = false;

  private constructor(options: BridgeOptions, thread: T3ThreadInfo, dir: string, socketPath: string, state: State) {
    this.options = options; this.client = options.client; this.thread = thread; this.threadId = thread.threadId;
    this.dir = dir; this.outbox = join(dir, "outbox"); this.socketPath = socketPath; this.state = state;
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
    if (thread.archived) throw new Error("This T3 thread is archived. Reopen it before reviewing.");
    const repo = await realpath((await git(thread.cwd, ["rev-parse", "--show-toplevel"])).trim());
    const branch = (await git(repo, ["symbolic-ref", "--quiet", "--short", "HEAD"]).catch(() => "")).trim();
    if (thread.branch !== null && thread.branch !== branch) {
      throw new Error(`This thread belongs to branch ${thread.branch}, but its checkout is on ${branch || "a detached HEAD"}. Restore the thread's branch before reviewing.`);
    }
    const id = digest(`${options.endpoint}\0${thread.threadId}\0${thread.cwd}${mode === "review" ? "\0review" : ""}`).slice(0, 24);
    const root = options.stateRoot ?? join(process.env.XDG_STATE_HOME || join(homedir(), ".local", "state"), "helix", "t3-review");
    const dir = join(root, id);
    await mkdir(dir, { recursive: true, mode: 0o700 });
    const previous = await readJson<State>(join(dir, "state.json"));
    if (previous && (previous.version !== 1 || previous.threadId !== thread.threadId || previous.cwd !== thread.cwd || previous.repo !== repo)) {
      throw new Error("The saved HX review binding does not match this thread and worktree.");
    }
    if (previous && previous.branch !== branch) throw new Error("This review belongs to a different branch. Restore that branch to resume its pending feedback.");
    const revision = options.baseline ?? previous?.baseline ?? "HEAD";
    const baseline = (await git(repo, ["rev-parse", "--verify", "--end-of-options", `${revision}^{commit}`])).trim();
    await git(repo, ["merge-base", "--is-ancestor", baseline, "HEAD"]).catch(() => {
      throw new Error("The review baseline is no longer in this branch. Reopen with --base <revision> to choose the review range.");
    });
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
    const bridge = new ReviewBridge(options, thread, dir, socketPath, state);
    await bridge.acquireLock();
    try {
      for (const name of ["outbox", "deliveries", "receipts"]) await mkdir(join(dir, name), { mode: 0o700, recursive: true });
      if (mode === "work" && !previous && !isActive(thread)) {
        await bridge.snapshot();
        bridge.state.lastRun = thread.latestRunId;
      }
      await bridge.save();
      return bridge;
    } catch (error) { await bridge.close(); throw error; }
  }

  private async acquireLock(): Promise<void> {
    const path = join(this.dir, "bridge.lock");
    for (let attempt = 0; attempt < 8; attempt++) {
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
          continue;
        }
        throw new Error("This thread already has an HX review open. Return to that editor or close it first.");
      }
      await writeFile(join(path, "owner.json"), JSON.stringify({ pid: process.pid, token: this.lockToken }), { flag: "wx", mode: 0o600 });
      return;
    }
    throw new Error("The HX review lock kept changing. Close other bridges before reopening.");
  }

  private save(): Promise<void> { return writeJson(join(this.dir, "state.json"), this.state); }

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
    await sendCommands(this.socketPath, [`echo ${quoteCommand(message)}`]).catch(() => undefined);
  }

  private async validateBinding(): Promise<T3ThreadInfo> {
    const thread = await this.client.readThread(this.threadId);
    if (thread.archived || thread.cwd !== this.state.cwd) throw new Error("The T3 thread was archived or moved to another worktree. Reopen Review in HX; feedback has been kept.");
    const branch = (await git(this.repo, ["symbolic-ref", "--quiet", "--short", "HEAD"]).catch(() => "")).trim();
    if (branch !== this.state.branch || (thread.branch !== null && thread.branch !== branch)) throw new Error("The checkout no longer matches this thread's branch. Restore that branch before sending feedback.");
    return thread;
  }

  async tick(): Promise<void> {
    if (this.closed) return;
    try {
      const thread = await this.validateBinding();
      const pending = (await readdir(this.outbox)).filter(name => name.endsWith(".json")).sort();
      for (const name of pending) {
        try { await this.deliver(name, true); }
        catch (error) { await this.announce(`T3: feedback kept — ${(error as Error).message}`); }
      }
      if (this.mode === "work" && !isActive(thread) && thread.latestRunId !== this.state.lastRun) {
        await this.snapshot(); this.state.lastRun = thread.latestRunId; await this.save();
        await this.announce(`T3: ${thread.title} — ${thread.status}; review updated`);
      }
      this.thread = thread;
      await this.refresh();
      for (const name of pending) {
        try { await this.deliver(name); }
        catch (error) { await this.announce(`T3: feedback kept — ${(error as Error).message}`); }
      }
    } catch (error) { await this.announce(`T3: ${(error as Error).message}`); }
  }

  async refresh(): Promise<void> {
    const info = await stat(this.socketPath).catch(error => {
      this.socketIdentity = undefined;
      throw error;
    });
    const identity = `${info.ino}:${info.ctimeMs}`;
    const newSocket = identity !== this.socketIdentity;
    const title = [...`T3: ${this.title.replace(/[\p{Cc}\u2028\u2029]/gu, " ")}`].slice(0, 120).join("");
    if (newSocket || title !== this.socketLabel) {
      await sendCommands(this.socketPath, [`review-handoff ${quoteCommand(this.outbox)} ${quoteCommand(title)}`,
        ...(this.mode === "work" ? [`review-baseline ${this.baseline}`] : [])]);
      if (newSocket && this.state.pullRequest) {
        await sendCommands(this.socketPath, [`review-pr-attach ${quoteCommand(this.state.pullRequest)}`]);
      }
      this.socketIdentity = identity;
      this.socketLabel = title;
      this.fingerprint = undefined;
      await this.announce(`${title} — connected; Space A a asks about your selection`);
    }
    if (this.mode === "review") return;
    const paths = await this.paths();
    const files = await Promise.all(paths.map(async path => {
      const value = await stat(path).catch(() => undefined);
      return `${path}:${value?.mtimeMs}:${value?.size}`;
    }));
    const fingerprint = digest((await git(this.repo, ["rev-parse", "HEAD"])) + files.join("\0") + await git(this.repo, ["diff", "--cached", "--raw", "--"]));
    if (fingerprint !== this.fingerprint) {
      await sendCommands(this.socketPath, [`review-baseline ${this.baseline}`]);
      this.fingerprint = fingerprint;
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
        const states = await batch.inspect();
        for (const [file, state] of states) {
          if (!state.problem) continue;
          try { await stat(file); }
          catch (error) { if ((error as NodeJS.ErrnoException).code === "ENOENT") continue; throw error; }
          throw new Error(`Cannot verify ${file}: ${state.problem}. Feedback is kept; no deletion has been inferred.`);
        }
        const { warnings } = batch.render(path, states);
        const handoff = await buildHandoff(batch, { repo: this.repo, snapshots: this.state.snapshots, baseline: this.baseline, strictCurrentReads: true });
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
    const path = join(this.dir, "bridge.lock");
    const ownerPath = join(path, "owner.json");
    const owner = await readJson<{ token: string }>(ownerPath);
    if (owner?.token === this.lockToken) { await rm(ownerPath); await rmdir(path); }
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
