import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { createServer } from "node:http";
import { createServer as createSocketServer } from "node:net";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { test } from "node:test";
import { hasReviewCommit, spawnReviewEditor, syncLoop } from "./cli.ts";

const execute = promisify(execFile);
async function environment(t: { after(fn: () => Promise<void>): void }, id: string) {
  const paths: string[] = [];
  const server = createServer((request, response) => {
    paths.push(request.url!);
    response.setHeader("content-type", "application/json");
    response.end(JSON.stringify({ environmentId: id }));
  });
  await new Promise<void>(done => server.listen(0, "127.0.0.1", done));
  t.after(() => new Promise<void>(done => server.close(() => done())));
  const address = server.address() as { port: number };
  return { endpoint: `http://127.0.0.1:${address.port}/mcp`, paths };
}
async function fixture(t: { after(fn: () => Promise<void>): void }) {
  const dir = await mkdtemp(join(tmpdir(), "hx-cli-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  await mkdir(join(dir, "userdata"));
  const env: NodeJS.ProcessEnv = { ...process.env, XDG_CONFIG_HOME: dir };
  for (const key of Object.keys(env)) if (/^(HX_T3_|T3_|T3CODE_)/u.test(key)) delete env[key];
  return { dir, env, async run(args: string[]) {
    return execute(process.execPath, ["contrib/t3-helix-review/cli.ts", ...args], { env, timeout: 5000 });
  } };
}

test("native launch requires authoritative terminal context instead of silently using global login", async t => {
  const f = await fixture(t);
  await assert.rejects(f.run(["--environment-id", "dev", "--thread", "thread"]), /terminal did not supply its environment/u);
});

test("invalid or conflicting editor modes fail before connecting", async t => {
  const f = await fixture(t);
  await assert.rejects(f.run(["--mode", "other"]), /mode must be work or review/u);
  await assert.rejects(f.run(["--mode", "review", "--location", JSON.stringify({ mode: "work" })]), /modes do not match/u);
  await assert.rejects(f.run(["--mode", "review", "--base", "HEAD"]), /PR review uses the PR's base and head/u);
});

test("native environment mismatch stops before pairing or reading the thread", async t => {
  const f = await fixture(t); const server = await environment(t, "other");
  await writeFile(join(f.dir, "userdata", "server-runtime.json"), JSON.stringify({ version: 1, origin: new URL(server.endpoint).origin }));
  await assert.rejects(f.run(["--environment-id", "dev", "--base-dir", f.dir, "--thread", "thread"]), /different T3 environment/u);
  assert.deepEqual(server.paths, ["/.well-known/t3/environment"]);
});

test("an explicit MCP credential cannot redirect a native launch to another environment", async t => {
  const f = await fixture(t); const dev = await environment(t, "dev"); const live = await environment(t, "live");
  await writeFile(join(f.dir, "userdata", "server-runtime.json"), JSON.stringify({ version: 1, origin: new URL(dev.endpoint).origin }));
  f.env.HX_T3_MCP_URL = live.endpoint; f.env.HX_T3_MCP_TOKEN = "must-not-send";
  await assert.rejects(f.run(["--environment-id", "dev", "--base-dir", f.dir, "--thread", "thread"]), /another T3 environment/u);
  assert.deepEqual(dev.paths, ["/.well-known/t3/environment"]);
  assert.deepEqual(live.paths, ["/.well-known/t3/environment"]);
});

function closedMarker(data: string): { nonce: string; payload: unknown } {
  const match = /^\x1b\]777;hx-review;([a-f\d-]+);([a-f\d]+)\x07$/u.exec(data);
  assert.ok(match, "one closed control marker");
  return { nonce: match[1], payload: JSON.parse(Buffer.from(match[2], "hex").toString("utf8")) };
}

test("each spawned HX gets a fresh env-only control token and a matching close on failure", async t => {
  const f = await fixture(t);
  const program = join(f.dir, "fake-hx.cjs");
  const capture = join(f.dir, "capture.json");
  await writeFile(program, `#!${process.execPath}\nrequire('node:fs').writeFileSync(${JSON.stringify(capture)}, JSON.stringify({token:process.env.HELIX_REVIEW_ACTION_TOKEN,socket:process.env.HELIX_COMMAND_SOCKET,deferred:process.env.HELIX_REVIEW_DEFER_READY,mode:process.env.HELIX_REVIEW_MODE,args:process.argv.slice(2)}));\nprocess.exit(7);\n`, { mode: 0o700 });
  const environment = { ...f.env, HELIX_REVIEW_ACTION_TOKEN: "inherited-old-token" };
  const nonces: string[] = [];
  for (let i = 0; i < 2; i++) {
    const markers: string[] = [];
    const mode = i === 0 ? "work" : "review";
    const editor = spawnReviewEditor(program, f.dir, "/private/hx.sock", environment, marker => markers.push(marker), mode);
    assert.equal(await editor.done, 7);
    assert.equal(markers.length, 1);
    const marker = closedMarker(markers[0]);
    const observed = JSON.parse(await readFile(capture, "utf8"));
    assert.match(observed.token, /^[a-f\d]{8}-[a-f\d]{4}-4[a-f\d]{3}-[89ab][a-f\d]{3}-[a-f\d]{12}$/u);
    assert.equal(marker.nonce, observed.token);
    assert.deepEqual(marker.payload, { v: 1, kind: "closed" });
    assert.equal(observed.socket, "/private/hx.sock");
    assert.equal(observed.deferred, "1");
    assert.equal(observed.mode, mode);
    assert.deepEqual(observed.args, ["-w", f.dir]);
    nonces.push(observed.token);
  }
  assert.notEqual(nonces[0], nonces[1]);
  assert.equal(environment.HELIX_REVIEW_ACTION_TOKEN, "inherited-old-token");
});

test("a killed or missing HX still closes its own control session without advertising readiness", async t => {
  const f = await fixture(t);
  const program = join(f.dir, "waiting-hx.cjs");
  await writeFile(program, `#!${process.execPath}\nsetInterval(() => {}, 1000);\n`, { mode: 0o700 });
  const markers: string[] = [];
  const editor = spawnReviewEditor(program, f.dir, "/private/hx.sock", f.env, marker => markers.push(marker));
  editor.child.kill("SIGKILL");
  assert.equal(await editor.done, null);
  assert.equal(markers.length, 1);
  assert.deepEqual(closedMarker(markers[0]).payload, { v: 1, kind: "closed" });

  const failedMarkers: string[] = [];
  const missing = spawnReviewEditor(join(f.dir, "missing-hx"), f.dir, "/private/hx.sock", f.env, marker => failedMarkers.push(marker));
  await assert.rejects(missing.done, /ENOENT/u);
  assert.equal(failedMarkers.length, 1);
  assert.deepEqual(closedMarker(failedMarkers[0]).payload, { v: 1, kind: "closed" });
  assert.notEqual(closedMarker(markers[0]).nonce, closedMarker(failedMarkers[0]).nonce);
});


test("basic editing fallback distinguishes non-Git and unborn repositories from reviewable history", async t => {
  const f = await fixture(t);
  assert.equal(await hasReviewCommit(f.dir), false);
  await execute("git", ["-C", f.dir, "init", "-q"]);
  assert.equal(await hasReviewCommit(f.dir), false);
  await execute("git", ["-C", f.dir, "-c", "user.name=Test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-qm", "first"]);
  assert.equal(await hasReviewCommit(f.dir), true);
  await assert.rejects(hasReviewCommit(join(f.dir, "missing-directory")), /cannot change to/u);
});

test("startup retries an asynchronous PR attachment before opening its file and advertising readiness", async t => {
  const f = await fixture(t);
  await execute("git", ["-C", f.dir, "init", "-q"]);
  await execute("git", ["-C", f.dir, "-c", "user.name=Test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-qm", "first"]);
  const socketPath = join(f.dir, "hx.sock");
  const commands: string[] = [];
  let loading = false;
  let opened = false;
  let attempts = 0;
  let child: ReturnType<typeof execFile>;
  const server = createSocketServer(socket => {
    let pending = "";
    socket.on("data", bytes => {
      const lines = (pending + bytes.toString()).split("\n");
      pending = lines.pop()!;
      for (const line of lines) {
        commands.push(line);
        if (line.startsWith("review-pr-attach ")) loading = true;
        if (line.startsWith("review-open ") && line.includes("file.txt")) {
          attempts++;
          if (loading) {
            loading = false;
            socket.write("error pull request is loading; navigation will be ready when checkout finishes\n");
            continue;
          }
          opened = true;
        }
        socket.write("ok\n");
        if (line.startsWith("review-launch-ready")) {
          assert(opened);
          child.kill("SIGTERM");
        }
      }
    });
  });
  await new Promise<void>(done => server.listen(socketPath, done));
  t.after(() => new Promise<void>(done => server.close(() => done())));
  const thread = { threadId: "thread", title: "Test", cwd: f.dir, branch: null, archived: false, linkedPullRequest: { url: "https://github.com/team/project/pull/7" } };
  const script = `
    import { T3Client } from './contrib/t3-helix-review/t3.ts';
    import { main } from './contrib/t3-helix-review/cli.ts';
    T3Client.prototype.readThread = async () => (${JSON.stringify(thread)});
    await main(${JSON.stringify(["--thread", "thread", "--mode", "review", "--attach", "--socket", socketPath, "--state-dir", join(f.dir, "state"), "--file", "file.txt"])});
  `;
  await new Promise<void>((done, fail) => {
    child = execFile(process.execPath, ["--input-type=module", "-e", script], {
      env: { ...f.env, HX_T3_MCP_URL: "http://127.0.0.1:1/mcp", HX_T3_MCP_TOKEN: "test" }, timeout: 10000,
    }, error => error ? fail(error) : done());
  });
  assert.equal(attempts, 2);
  assert.equal(commands.filter(command => command.startsWith("review-pr-attach ")).length, 1);
  assert.equal(commands.filter(command => command.startsWith("review-launch-ready ")).length, 1);
});

async function fakeEditor(t: { after(fn: () => Promise<void>): void }, path: string, onLine: (line: string) => void = () => {}) {
  const commands: string[] = [];
  const server = createSocketServer(socket => {
    let pending = "";
    socket.on("data", bytes => {
      const lines = (pending + bytes.toString()).split("\n");
      pending = lines.pop()!;
      for (const line of lines) { commands.push(line); socket.write("ok\n"); onLine(line); }
    });
  });
  await new Promise<void>(done => server.listen(path, done));
  t.after(() => new Promise<void>(done => server.close(() => done())));
  return { commands, server };
}

test("an attached bridge leaves navigation to its client and exits cleanly once the editor is gone", async t => {
  const f = await fixture(t);
  const repo = join(f.dir, "repo");
  await mkdir(repo);
  await execute("git", ["-C", repo, "init", "-q", "-b", "main"]);
  await execute("git", ["-C", repo, "-c", "user.name=Test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-qm", "first"]);
  const socketPath = join(f.dir, "hx.sock");
  // The editor quits after the bridge's startup commands.
  const editor = await fakeEditor(t, socketPath, line => { if (line.startsWith("review-launch-ready")) editor.server.close(); });
  const thread = { threadId: "thread", title: "Test", status: "completed", latestRunId: null, cwd: repo, branch: "main", archived: false };
  const script = `
    import { T3Client } from './contrib/t3-helix-review/t3.ts';
    import { main } from './contrib/t3-helix-review/cli.ts';
    T3Client.prototype.readThread = async () => (${JSON.stringify(thread)});
    await main(${JSON.stringify(["--thread", "thread", "--attach", "--socket", socketPath, "--state-dir", join(f.dir, "state")])});
  `;
  await execute(process.execPath, ["--input-type=module", "-e", script], {
    env: { ...f.env, HX_T3_MCP_URL: "http://127.0.0.1:1/mcp", HX_T3_MCP_TOKEN: "test" }, timeout: 15000,
  });
  assert.equal(editor.commands[0], `review-open '{"mode":"work"}'`);
  assert(editor.commands.some(command => command.startsWith("review-handoff ")));
  assert(!editor.commands.some(command => command === "review-next" || command.startsWith("review-open '{\"filePath\"")));
});

test("an attached bridge without a saved login says so in the editor instead of pairing", async t => {
  const f = await fixture(t);
  const socketPath = join(f.dir, "hx.sock");
  const editor = await fakeEditor(t, socketPath);
  await assert.rejects(f.run(["--attach", "--socket", socketPath, "--thread", "thread"]), /No saved T3 connection/u);
  assert.equal(editor.commands.length, 1);
  assert.match(editor.commands[0], /^echo 'hx-t3: No saved T3 connection\. Run hx-t3 setup in a T3 terminal/u);
});

test("an idle attached bridge syncs less often, catches up on a change and still notices its editor leave", async t => {
  const outbox = await mkdtemp(join(tmpdir(), "hx-outbox-"));
  t.after(() => rm(outbox, { recursive: true, force: true }));
  const changes = [false, false, false, false, false, true, false, false];
  const ticks: number[] = [];
  const sleeps: number[] = [];
  let checks = 0;
  await syncLoop({ signal: new AbortController().signal, attach: true, outbox,
    listening: async () => { checks++; return ticks.length < changes.length; },
    open: async () => true,
    tick: async () => { ticks.push(checks); return changes[ticks.length - 1]; },
    sleep: async ms => { sleeps.push(ms); } });
  // Checks between syncs double up to the 15 s cap and drop back after a change.
  assert.deepEqual(ticks.slice(1).map((check, i) => check - ticks[i]), [2, 4, 8, 10, 10, 1, 2]);
  // The editor still counts as gone after three 1.5 s checks without it.
  assert.equal(checks - ticks.at(-1)!, 3);
  assert(sleeps.every(ms => ms === 1500));
});

test("a batch landing in the outbox wakes a waiting bridge at once", { timeout: 10_000 }, async t => {
  const outbox = await mkdtemp(join(tmpdir(), "hx-outbox-"));
  t.after(() => rm(outbox, { recursive: true, force: true }));
  const controller = new AbortController();
  let opened!: () => void;
  const ready = new Promise<void>(done => { opened = done; });
  let synced!: () => void;
  const ticked = new Promise<void>(done => { synced = done; });
  const loop = syncLoop({ signal: controller.signal, attach: true, outbox,
    listening: async () => true,
    open: async () => { opened(); return true; },
    tick: async () => { synced(); return false; },
    // No time passes: only the outbox, or stopping, ends a wait.
    sleep: (_ms, signal) => new Promise<void>(done => {
      if (signal.aborted) return done();
      signal.addEventListener("abort", () => done(), { once: true });
    }) });
  await ready;
  await writeFile(join(outbox, "batch.json"), "{}");
  await ticked;
  controller.abort();
  await loop;
});
