import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { createServer } from "node:http";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { test } from "node:test";
import { hasReviewCommit, spawnReviewEditor } from "./cli.ts";

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
  await writeFile(program, `#!${process.execPath}\nrequire('node:fs').writeFileSync(${JSON.stringify(capture)}, JSON.stringify({token:process.env.HELIX_REVIEW_ACTION_TOKEN,socket:process.env.HELIX_COMMAND_SOCKET,deferred:process.env.HELIX_REVIEW_DEFER_READY,args:process.argv.slice(2)}));\nprocess.exit(7);\n`, { mode: 0o700 });
  const environment = { ...f.env, HELIX_REVIEW_ACTION_TOKEN: "inherited-old-token" };
  const nonces: string[] = [];
  for (let i = 0; i < 2; i++) {
    const markers: string[] = [];
    const editor = spawnReviewEditor(program, f.dir, "/private/hx.sock", environment, marker => markers.push(marker));
    assert.equal(await editor.done, 7);
    assert.equal(markers.length, 1);
    const marker = closedMarker(markers[0]);
    const observed = JSON.parse(await readFile(capture, "utf8"));
    assert.match(observed.token, /^[a-f\d]{8}-[a-f\d]{4}-4[a-f\d]{3}-[89ab][a-f\d]{3}-[a-f\d]{12}$/u);
    assert.equal(marker.nonce, observed.token);
    assert.deepEqual(marker.payload, { v: 1, kind: "closed" });
    assert.equal(observed.socket, "/private/hx.sock");
    assert.equal(observed.deferred, "1");
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
