#!/usr/bin/env node
import { execFile, spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { chmod, mkdir, mkdtemp, rm, readFile, realpath, writeFile } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import { createInterface } from "node:readline/promises";
import { setTimeout as delay } from "node:timers/promises";
import { ReviewBridge, editorListening, quoteCommand, sendCommands, writeJson } from "./bridge.ts";
import type { ReviewMode } from "./bridge.ts";
import { T3Client, connectionFromEnvironment, defaultT3BaseDir, discoverLocalEndpoint, readEnvironment } from "./t3.ts";
import type { T3Connection, T3ThreadInfo } from "./t3.ts";
import { pairLocalClient } from "./auth.ts";

const help = `Work together or review a pull request in Helix.

  hx-t3                         Pick a thread for this checkout and open HX
  hx-t3 --thread <id-or-link>    Open a particular thread's worktree
  hx-t3 --base main             Review from a commit, retaining it across turns
  hx-t3 --pr <url>              Review the checked-out PR without changing branches
  hx-t3 --mode review           Review the thread's linked PR using suggestion drafts
  hx-t3 --file src/main.rs      Start at a file in the thread's worktree
  hx-t3 setup                   Connect this machine to its running T3 server
  hx-t3 install                 Install the hx-t3 launcher in ~/.local/bin
  hx-t3 pending                 List feedback waiting to be delivered
  hx-t3 discard --batch <name>  Cancel a pending batch (keeps an archive)

In HX: Space A a asks about the selection; Space A c adds a comment; Space A f picks a changed file.
Work together: save corrections, then Space A S (:review-send [instruction]) sends them
with your comments, or Space A P (:review-send-pr [summary]) has the agent post them as a PR review.
Review a PR: edit suggestion buffers without saving over source files.
Ask agent discusses your selection in either mode; saving alone never starts a turn.
Answers appear in the T3 thread. Quitting HX stops the bridge.

Options:
  --thread <id>    Also accepts a t3-thread://v1/<id> link
  --base <rev>     Default: HEAD on first use; retained on subsequent opens
  --mode <mode>    work (default) or review; each keeps a separate editor session
  --pr <url>       GitHub PR to review; --mode review uses the thread's linked PR
  --file <path>    Relative to the bound worktree; basic editing also works before the first Git commit
  --hx <program>   HX executable (default: hx or HELIX_BIN)
  --attach        Attach to an existing editor instead of launching HX; exits when it closes
  --socket <path>  Override this thread's private HX socket
  --state-dir     Override the private review state directory
  --config <path>  Override the private connection file
  --t3 <program>  T3 CLI used for local setup
  --base-dir      T3 home directory (default: T3CODE_HOME or ~/.t3)
  --environment-id <id>  Require this T3 environment (set by Review in HX)
  --batch <name>   Batch from pending to cancel with discard
  --help          Show this help

Connection setup uses the local T3 CLI and stores a private 30-day MCP login.
Run setup again after it expires; revoke Helix review in T3 Connections.
Remote servers: run hx-t3 on the workspace host, or provide HX_T3_MCP_URL
and HX_T3_MCP_TOKEN. No provider-specific extension is required.
`;

export function threadIdFromArgument(value: string): string {
  const id = value.startsWith("t3-thread://v1/") ? value.slice("t3-thread://v1/".length) : value;
  if (!/^[\w.:%-]{1,2048}$/u.test(id)) throw new Error("Use a T3 thread ID or its t3-thread://v1/ link.");
  return id;
}

export function configPath(environmentId?: string, endpoint?: string): string {
  const name = environmentId ? `t3-review-${createHash("sha256").update(`${environmentId}\0${endpoint}`).digest("hex").slice(0, 24)}.json` : "t3-review.json";
  return join(process.env.XDG_CONFIG_HOME || join(homedir(), ".config"), "helix", name);
}

async function loadConnection(path: string): Promise<T3Connection | undefined> {
  const env = connectionFromEnvironment();
  if (env) return env;
  let text: string;
  try { text = await readFile(path, "utf8"); }
  catch (error) { if ((error as NodeJS.ErrnoException).code === "ENOENT") return undefined; throw error; }
  const config = JSON.parse(text);
  if (typeof config.endpoint !== "string" || typeof config.authorization !== "string") throw new Error("Invalid connection file. Run hx-t3 setup to reconnect.");
  if (config.expiresAt && Date.parse(config.expiresAt) <= Date.now()) throw new Error("Your T3 login expired. Run hx-t3 setup to reconnect; pending feedback is kept.");
  return config;
}

export async function chooseThread(client: T3Client, cwd: string): Promise<string> {
  const canonical = await realpath(cwd);
  const projects = await client.listProjects();
  const matching = projects.filter(project => canonical === project.workspaceRoot || canonical.startsWith(project.workspaceRoot + "/"));
  const candidates: T3ThreadInfo[] = [];
  for (const project of matching.length ? matching : projects) {
    let cursor: number | undefined;
    const seen = new Set<number>();
    do {
      const page = await client.listThreads({ projectId: project.id, cursor, includeSubagents: false });
      for (const thread of page.threads) {
        if (thread.settled) continue;
        const detail = await client.readThread(thread.threadId).catch(() => undefined);
        if (detail && !detail.archived && (detail.cwd === canonical || canonical.startsWith(detail.cwd + "/"))) candidates.push(detail);
      }
      if (page.nextCursor === null) break;
      cursor = page.nextCursor;
      if (seen.has(cursor)) throw new Error("T3 returned a repeated thread page.");
      seen.add(cursor);
    } while (candidates.length < 100);
  }
  if (!candidates.length) throw new Error("No active T3 thread uses this checkout. Open its thread in T3 or pass --thread <id-or-link>.");
  if (candidates.length === 1) return candidates[0].threadId;
  if (!process.stdin.isTTY) throw new Error("Several T3 threads share this checkout. Pass --thread <id-or-link> to choose the destination.");
  console.log("Review which T3 thread?\n");
  candidates.forEach((thread, i) => console.log(`  ${i + 1}. ${thread.title || "Untitled"} (${thread.status})`));
  const input = createInterface({ input: process.stdin, output: process.stdout });
  try {
    const answer = await input.question("\nThread number (Enter to cancel): ");
    if (!/^\d+$/u.test(answer) || !candidates[Number(answer) - 1]) throw new Error("No thread selected.");
    return candidates[Number(answer) - 1].threadId;
  } finally { input.close(); }
}

async function install(): Promise<void> {
  const dir = join(homedir(), ".local", "bin");
  await mkdir(dir, { recursive: true });
  const quote = (value: string) => `'${value.replaceAll("'", "'\\''")}'`;
  const path = join(dir, "hx-t3");
  const script = `#!/bin/sh\n# Helix review for T3 Code\nexec ${quote(process.execPath)} ${quote(fileURLToPath(import.meta.url))} "$@"\n`;
  let previous: string | undefined;
  try { previous = await readFile(path, "utf8"); } catch (error) { if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error; }
  if (previous && !previous.startsWith("#!/bin/sh\n# Helix review for T3 Code\n")) throw new Error(`${path} already exists and is not this launcher's file.`);
  await writeFile(path, script, { mode: 0o755 });
  await chmod(path, 0o755);
  console.log(`Installed ${path}. Run hx-t3 from a T3 terminal, or use Review in HX.`);
}

export function spawnReviewEditor(program: string, cwd: string, socketPath: string,
  environment = process.env, writeControl: (data: string) => void = data => { process.stdout.write(data); }, mode: ReviewMode = "work") {
  const nonce = randomUUID();
  const child = spawn(program, ["-w", cwd], { stdio: "inherit", env: {
    ...environment, HELIX_COMMAND_SOCKET: socketPath, HELIX_REVIEW_ACTION_TOKEN: nonce, HELIX_REVIEW_DEFER_READY: "1", HELIX_REVIEW_MODE: mode,
  } });
  let closed = false;
  const emitClosed = () => {
    if (closed) return;
    closed = true;
    const payload = Buffer.from(JSON.stringify({ v: 1, kind: "closed" }), "utf8").toString("hex");
    writeControl(`\x1b]777;hx-review;${nonce};${payload}\x07`);
  };
  const done = new Promise<number | null>((resolve, reject) => {
    child.once("exit", code => { emitClosed(); resolve(code); });
    child.once("error", error => { emitClosed(); reject(error); });
  });
  return { child, done };
}

export async function hasReviewCommit(cwd: string): Promise<boolean> {
  const git = (args: string[]) => new Promise<string>((done, fail) => {
    execFile("git", ["-C", cwd, ...args], { encoding: "utf8" }, (error, stdout, stderr) => {
      if (error) fail(Object.assign(error, { stderr })); else done(stdout.trim());
    });
  });
  try { await git(["rev-parse", "--show-toplevel"]); }
  catch (error) {
    const failure = error as NodeJS.ErrnoException & { stderr?: string };
    if (failure.code === "ENOENT" || /not a git repository/iu.test(failure.stderr ?? "")) return false;
    throw error;
  }
  try { await git(["rev-parse", "--verify", "HEAD^{commit}"]); return true; }
  catch (error) {
    // A missing symbolic branch is an unborn checkout; other HEAD failures stay errors.
    const branch = await git(["symbolic-ref", "--quiet", "HEAD"]).catch(() => undefined);
    if (!branch) throw error;
    try { await git(["show-ref", "--verify", "--quiet", branch]); }
    catch (missing) { if ((missing as { code?: number }).code === 1) return false; throw error; }
    throw error;
  }
}

async function openPlainEditor(program: string, cwd: string, location: object, attach: boolean, requestedSocket?: string) {
  const dir = await mkdtemp(join(tmpdir(), "hx-t3-edit-"));
  const socket = requestedSocket ?? join(dir, "hx.sock");
  let editor: ReturnType<typeof spawnReviewEditor> | undefined;
  let ended = false;
  let stopped = false;
  let launchError: Error | undefined;
  const stop = () => { stopped = true; editor?.child.kill("SIGTERM"); };
  process.once("SIGINT", stop); process.once("SIGTERM", stop);
  try {
    if (!attach) {
      if (!process.stdin.isTTY || !process.stdout.isTTY) throw new Error("HX needs a terminal. Run this action in a T3 terminal.");
      editor = spawnReviewEditor(program, cwd, socket);
      void editor.done.then(() => { ended = true; }, error => { ended = true; launchError = error; });
    }
    while (true) {
      if (ended || stopped) throw launchError ?? new Error("HX exited before opening the requested file.");
      try { await sendCommands(socket, [`review-open ${quoteCommand(JSON.stringify(location))}`, "review-launch-ready"]); break; }
      catch (error) {
        if (ended || stopped) throw launchError ?? new Error("HX exited before opening the requested file.");
        if (!/ENOENT|ECONNREFUSED|did not acknowledge|closed before acknowledging/u.test((error as Error).message)) throw error;
        if (attach) throw error;
        await delay(100);
      }
    }
    if (editor) { const code = await editor.done; if (code) throw new Error(`HX exited with status ${code}.`); }
  } finally {
    process.removeListener("SIGINT", stop); process.removeListener("SIGTERM", stop);
    if (editor && editor.child.exitCode === null && editor.child.signalCode === null) editor.child.kill("SIGTERM");
    await rm(dir, { recursive: true, force: true });
  }
}

// Consecutive 1.5 s ticks without a listening socket after which an attached editor counts as
// gone; one starting alongside this process gets longer to bind its socket.
const EDITOR_GONE_TICKS = 3;
const EDITOR_START_TICKS = 20;

function parseOptions(args: string[]) {
  return parseArgs({ args, allowPositionals: true, options: {
    help: { type: "boolean", short: "h" }, thread: { type: "string" }, base: { type: "string" }, pr: { type: "string" }, mode: { type: "string" }, file: { type: "string" }, line: { type: "string" }, column: { type: "string" }, worktree: { type: "string" }, revision: { type: "string" }, location: { type: "string" },
    hx: { type: "string" }, attach: { type: "boolean" }, socket: { type: "string" }, "state-dir": { type: "string" },
    config: { type: "string" }, t3: { type: "string" }, "base-dir": { type: "string" },
    "environment-id": { type: "string" }, batch: { type: "string" },
  } });
}

export async function main(args = process.argv.slice(2)): Promise<void> {
  const options = parseOptions(args);
  try { await run(options); }
  catch (error) {
    // An attached bridge runs in a terminal nobody may be watching; say why in the editor.
    const socket = options.values.attach ? options.values.socket : undefined;
    const reason = `hx-t3: ${(error as Error).message}`.replace(/[\p{Cc}\u2028\u2029]/gu, " ").slice(0, 500);
    if (socket) await sendCommands(socket, [`echo ${quoteCommand(reason)}`]).catch(() => undefined);
    throw error;
  }
}

async function run({ values, positionals }: ReturnType<typeof parseOptions>): Promise<void> {
  if (values.help) { console.log(help); return; }
  if (process.platform === "win32") throw new Error("HX review requires a Unix command socket. Run hx-t3 in WSL or on your Linux/macOS workspace host.");
  const location = values.location ? JSON.parse(values.location) : {
    ...(values.file ? { filePath: values.file } : {}),
    ...(values.line ? { line: Number(values.line) } : {}),
    ...(values.column ? { column: Number(values.column) } : {}),
    ...(values.worktree ? { worktreePath: values.worktree } : {}),
    ...(values.base && values.revision ? { baseline: values.base } : {}),
    ...(values.revision ? { revision: values.revision } : {}),
  };
  if (!location || typeof location !== "object" || Array.isArray(location)) throw new Error("Invalid HX location.");
  const mode = values.mode ?? location.mode ?? (values.pr ? "review" : "work");
  if (mode !== "work" && mode !== "review") throw new Error("HX mode must be work or review.");
  if (location.mode !== undefined && location.mode !== mode) throw new Error("The requested HX modes do not match.");
  if (mode === "review" && !location.revision && (location.baseline || values.base)) throw new Error("PR review uses the PR's base and head. Open Work together for other comparisons.");
  location.mode = mode;
  const command = positionals[0] ?? "open";
  if (positionals.length > 1 || !["open", "install", "setup", "pending", "discard"].includes(command)) throw new Error("Expected open, install, setup, pending, or discard. See hx-t3 --help.");
  if (command === "install") { await install(); return; }
  const baseDir = values["base-dir"] ?? defaultT3BaseDir();
  const runtimeStatePath = values["base-dir"] ? undefined : process.env.T3CODE_SERVER_RUNTIME_PATH;
  let environmentId = values["environment-id"];
  let endpoint: string | undefined;
  if (environmentId || values["base-dir"] || runtimeStatePath) {
    if (environmentId && !runtimeStatePath && !values["base-dir"]) throw new Error("The T3 terminal did not supply its environment. Relaunch Review in HX from the updated T3 server, or pass --base-dir explicitly.");
    endpoint = await discoverLocalEndpoint(baseDir, runtimeStatePath);
    const actual = await readEnvironment(endpoint);
    if (environmentId && environmentId !== actual.environmentId) throw new Error("This terminal belongs to a different T3 environment. Reopen Review in HX in the intended environment.");
    environmentId = actual.environmentId;
  }
  const path = values.config ?? configPath(environmentId, endpoint);
  let connection = command === "setup" ? undefined : await loadConnection(path);
  if (connection && environmentId) {
    const actual = await readEnvironment(connection.endpoint);
    if (actual.environmentId !== environmentId) throw new Error("The configured MCP connection belongs to another T3 environment. Remove conflicting MCP environment variables or run hx-t3 setup with the intended --base-dir.");
  }
  if (!connection) {
    if (values.attach) throw new Error("No saved T3 connection. Run hx-t3 setup in a T3 terminal, then open the file in T3 again.");
    console.log("Connecting Helix review to your local T3 server…");
    connection = await pairLocalClient({ baseDir, endpoint, runtimeStatePath, environmentId,
      cliPath: values.t3 ?? process.env.T3CODE_CLI_PATH ?? join(homedir(), ".t3", "bin", "t3") });
    await mkdir(dirname(path), { recursive: true, mode: 0o700 });
    await writeJson(path, connection);
    console.log("Connected. You can revoke Helix review in T3 Settings → Connections.");
  }
  if (command === "setup") return;
  const client = new T3Client(connection);
  const explicitThread = values.thread ?? process.env.T3CODE_THREAD_ID;
  const threadId = explicitThread ? threadIdFromArgument(explicitThread) : await chooseThread(client, process.cwd());
  if (location.worktreePath) {
    const thread = await client.readThread(threadId);
    if (await realpath(location.worktreePath) !== await realpath(thread.cwd)) throw new Error("The requested location belongs to another worktree.");
  }
  const boundThread = await client.readThread(threadId);
  if (boundThread.archived) throw new Error("This T3 thread is archived. Reopen it before editing.");
  if (command === "open" && !await hasReviewCommit(boundThread.cwd)) {
    if (mode === "review" || location.baseline || location.revision || values.base || values.pr)
      throw new Error("Review comparisons need a Git repository with a commit. Open a current file without a comparison to edit it.");
    console.log(`Editing ${boundThread.title || "Untitled"} in ${boundThread.cwd} · review feedback becomes available after the first Git commit`);
    await openPlainEditor(values.hx ?? process.env.HELIX_BIN ?? "hx", boundThread.cwd, location, values.attach ?? false, values.socket);
    return;
  }
  const bridge = await ReviewBridge.open({ client, endpoint: connection.endpoint, threadId,
    mode, baseline: location.revision ? undefined : values.base, pullRequest: values.pr, stateRoot: values["state-dir"], socketPath: values.socket,
    // A restarted sidecar waits out its predecessor's shutdown.
    lockWait: values.attach ? 10_000 : 0,
    onStatus: values.attach ? message => console.log(message) : undefined });
  if (command === "pending" || command === "discard") {
    try {
      if (command === "discard") {
        if (!values.batch) throw new Error("Choose a batch from hx-t3 pending, then use hx-t3 discard --batch <name>.");
        await bridge.discard(values.batch);
        console.log(`Cancelled ${values.batch}. The original feedback is retained in the review archive.`);
      } else {
        const pending = await bridge.pending();
        console.log(pending.length ? pending.join("\n") : "No pending feedback for this thread.");
      }
    } finally { await bridge.close(); }
    return;
  }
  const controller = new AbortController();
  let child: ReturnType<typeof spawn> | undefined;
  let childDone: Promise<number | null> | undefined;
  const stop = () => { controller.abort(); child?.kill("SIGTERM"); };
  process.once("SIGINT", stop); process.once("SIGTERM", stop);
  try {
    const thread = await client.readThread(threadId);
    console.log(mode === "review"
      ? `Reviewing ${bridge.pullRequest} · suggestion drafts leave checkout files unchanged`
      : `Working together on ${thread.title || "Untitled"} in ${thread.cwd}\nBaseline ${bridge.baseline.slice(0, 8)} · feedback returns to this T3 thread`);
    if (!values.attach) {
      if (!process.stdin.isTTY || !process.stdout.isTTY) throw new Error("HX needs a terminal. Run this action in a T3 terminal, or use --attach with an existing HX socket.");
      const editor = spawnReviewEditor(values.hx ?? process.env.HELIX_BIN ?? "hx", thread.cwd, bridge.socketPath, process.env, undefined, mode);
      child = editor.child;
      childDone = editor.done.finally(() => controller.abort());
      // Keep the rejection observed while the bridge waits for the socket.
      void childDone.catch(() => {});
    }
    let opened = false;
    let answered = false;
    let misses = 0;
    while (!controller.signal.aborted) {
      if (values.attach && !await editorListening(bridge.socketPath)) {
        if (++misses >= (answered ? EDITOR_GONE_TICKS : EDITOR_START_TICKS)) break;
      } else {
        answered = true; misses = 0;
        if (!opened) {
          try {
            if (values.attach) await sendCommands(bridge.socketPath, [`review-open ${quoteCommand(JSON.stringify({ mode }))}`]);
            await bridge.refresh();
            if (Object.keys(location).some(key => key !== "mode")) {
              if (location.worktreePath && await realpath(location.worktreePath) !== await realpath(thread.cwd)) throw new Error("The requested location belongs to another worktree.");
              await sendCommands(bridge.socketPath, [`review-open ${quoteCommand(JSON.stringify(location))}`]);
            } else if (!values.attach) {
              // An attached editor's client chooses what it shows.
              await sendCommands(bridge.socketPath, ["review-next"]).catch(error => {
                if (!/no (uncommitted )?changes/u.test(error.message)) throw error;
                return sendCommands(bridge.socketPath, [`echo ${quoteCommand("No changes in this review range. Space A b chooses an earlier baseline.")}`]);
              });
            }
            await sendCommands(bridge.socketPath, [`review-launch-ready${bridge.pullRequest ? ` ${quoteCommand(bridge.pullRequest)}` : ""}`]);
            opened = true;
          } catch (error) {
            // HX may still be loading its runtime or refreshing a large review.
            // Keep the editor open while these read-only startup commands retry.
            if (!/ENOENT|ECONNREFUSED|did not acknowledge|closed before acknowledging|pull request is loading; navigation will be ready when checkout finishes/u.test((error as Error).message)) throw error;
          }
        } else await bridge.tick();
      }
      await delay(1500, undefined, { signal: controller.signal }).catch(() => {});
    }
    if (childDone) {
      const code = await childDone;
      if (code) throw new Error(`HX exited with status ${code}.`);
    }
  } finally {
    process.removeListener("SIGINT", stop); process.removeListener("SIGTERM", stop);
    controller.abort();
    if (child && child.exitCode === null && child.signalCode === null) child.kill("SIGTERM");
    await bridge.close();
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch(error => { console.error(`hx-t3: ${(error as Error).message}`); process.exitCode = 1; });
}
