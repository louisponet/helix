import assert from "node:assert/strict";
import { test } from "node:test";
import { createServer } from "node:net";
import { mkdtemp, mkdir, readFile, readdir, rename, rm, stat, writeFile } from "node:fs/promises";
import { join } from "node:path";
import fs from "node:fs/promises";
import { syncBuiltinESMExports } from "node:module";
import { tmpdir } from "node:os";
import { ReviewBridge, git, quoteCommand, sendCommands } from "./bridge.ts";
import type { ReviewClient } from "./bridge.ts";
import type { T3ThreadInfo, T3SendResult } from "./t3.ts";
import { threadIdFromArgument } from "./cli.ts";

async function fixture(t: { after(fn: () => Promise<void>): void }) {
  const dir = await mkdtemp(join(tmpdir(), "hx-t3-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const repo = join(dir, "repo"); await mkdir(repo);
  await git(repo, ["init", "-b", "main"]);
  await git(repo, ["config", "user.name", "Review test"]);
  await git(repo, ["config", "user.email", "review@example.test"]);
  await git(repo, ["config", "commit.gpgsign", "false"]);
  await writeFile(join(repo, "file.txt"), "keep\nold check\nend\n");
  await git(repo, ["add", "."]); await git(repo, ["commit", "-m", "baseline"]);
  const baseline = (await git(repo, ["rev-parse", "HEAD"])).trim();
  const blob = (await git(repo, ["rev-parse", "HEAD:file.txt"])).trim();
  const thread: T3ThreadInfo = { threadId: "thread-1", projectId: "project", title: "Fix 'parser' 100%", status: "completed", activeRunId: null, latestRunId: "run-1", branch: "main", worktreePath: null, cwd: repo, archived: false, settled: false, snoozed: false, recentRuns: [] };
  const calls: { id: string; message: string; thread: string; mode: "auto" | "queue" }[] = [];
  let failure = false;
  const linked: string[] = [];
  const client: ReviewClient = {
    readThread: async () => ({ ...thread }),
    linkedPullRequests: async () => [...linked],
    send: async (id, message, request, mode = "auto") => {
      calls.push({ id: request, message, thread: id, mode });
      if (failure) throw new Error("connection lost after acceptance");
      return { threadId: id, messageId: "message", runId: "run-2", status: mode === "queue" ? "queued" : "running", delivery: mode === "queue" ? "queued" : "started" };
    },
  };
  const commands: string[] = [];
  let attachError: string | undefined;
  const socketPath = join(dir, "hx.sock");
  const server = createServer(socket => {
    let pending = "";
    socket.on("data", bytes => {
      const lines = (pending + bytes.toString()).split("\n"); pending = lines.pop()!;
      for (const line of lines) {
        commands.push(line);
        socket.write(attachError && line.startsWith("review-pr-attach ") ? `error ${attachError}\n` : "ok\n");
      }
    });
  });
  await new Promise<void>(done => server.listen(socketPath, done));
  t.after(() => new Promise<void>((done, fail) => server.close(error => error ? fail(error) : done())));
  const options = { client, threadId: thread.threadId, endpoint: "http://localhost/mcp", stateRoot: join(dir, "state"), socketPath };
  let bridge = await ReviewBridge.open(options); t.after(() => bridge.close());
  const batch = async (name = "review.json", request: unknown = { kind: "ask" }) => {
    await writeFile(join(bridge.outbox, name), JSON.stringify({ format: "helix-review-batch", version: 2,
      files: [{ path: join(repo, "file.txt"), baseline: { kind: "git-commit", commit_oid: baseline, blob_oid: blob } }],
      comments: [{ path: join(repo, "file.txt"), baseline: { kind: "git-commit", commit_oid: baseline, blob_oid: blob }, side: "old", start_line: 2, end_line: 2, current_line: 2, selected_text: "old check", context_before: "keep\n", context_after: "\nend\n", comment: "Why remove this check?" }], request }));
  };
  const writeBatch = async (name: string, { files = [{ path: join(repo, "file.txt"), baseline: { kind: "git-commit", commit_oid: baseline, blob_oid: blob } }], comments = [], request }: { files?: unknown[]; comments?: unknown[]; request?: unknown } = {}) => {
    await writeFile(join(bridge.outbox, name), JSON.stringify({ format: "helix-review-batch", version: 2, files, comments, ...(request ? { request } : {}) }));
  };
  return { dir, repo, baseline, blob, thread, linked, client, calls, commands, options, batch, writeBatch,
    get bridge() { return bridge; }, fail(value: boolean) { failure = value; },
    failAttach(message: string) { attachError = message; },
    async reopen(mode: "work" | "review" = "work") { await bridge.close(); bridge = await ReviewBridge.open({ ...options, mode }); return bridge; } };
}

function handoffDetails(message: string) {
  const quoted = /Handoff:\n(`+)\n([\s\S]*?)\n\1(?:\n|$)/u.exec(message);
  assert.ok(quoted, "delivery should contain the structured handoff");
  return JSON.parse(quoted[2]);
}

test("connects with a visible thread binding, refreshes edits, and retains baseline across commits/restarts", async t => {
  const f = await fixture(t);
  await f.bridge.tick();
  assert(f.commands.some(command => command.startsWith("review-handoff '") && command.endsWith("'T3: Fix ''parser'' 100%'")));
  await writeFile(join(f.repo, "file.txt"), "keep\nnew check\nend\n");
  await f.bridge.tick();
  await git(f.repo, ["add", "."]); await git(f.repo, ["commit", "-m", "agent change"]);
  f.thread.latestRunId = "run-2";
  await f.bridge.tick();
  assert.equal(f.bridge.baseline, f.baseline);
  await f.reopen(); assert.equal(f.bridge.baseline, f.baseline);
  assert(f.commands.filter(command => command.startsWith("review-baseline ")).every(command => command === `review-baseline ${f.baseline}`));
});

test("review mode attaches a linked PR once and asks the agent for proposals without checkout edits", async t => {
  const f = await fixture(t);
  f.thread.linkedPullRequest = { url: "https://github.com/team/project/pull/7", repository: "team/project", number: 7 };
  await f.reopen("review");
  const index = await readFile(join(f.repo, ".git/index"));
  await f.bridge.tick();
  await f.bridge.tick();
  f.thread.title = "Renamed while reviewing a conversation checkpoint";
  await f.bridge.tick();
  assert(f.commands.some(command => command.endsWith("'T3: Renamed while reviewing a conversation checkpoint'")));
  assert.deepEqual(f.commands.filter(command => command.startsWith("review-pr-attach ")), ["review-pr-attach 'https://github.com/team/project/pull/7'"]);
  assert.deepEqual(await readFile(join(f.repo, ".git/index")), index);
  assert.equal((await git(f.repo, ["rev-parse", "HEAD"])).trim(), f.baseline);
  await writeFile(join(f.repo, "file.txt"), "private unrelated working change\n");
  await f.batch(); await f.bridge.tick();
  assert.match(f.calls[0].message, /co-reviewer/u);
  assert(f.calls[0].message.includes(`at commit ${f.baseline}`));
  assert.match(f.calls[0].message, /Do not edit checkout files/u);
  assert.match(f.calls[0].message, /old check/u);
  assert.doesNotMatch(f.calls[0].message, /private unrelated working change/u);
  assert.equal(await readFile(join(f.repo, "file.txt"), "utf8"), "private unrelated working change\n");
});

test("startup waits for its command socket before treating the PR binding as refreshed", async t => {
  const f = await fixture(t);
  f.thread.linkedPullRequest = { url: "https://github.com/team/project/pull/7", repository: "team/project", number: 7 };
  await f.reopen("review");
  await rename(f.options.socketPath, `${f.options.socketPath}.waiting`);
  await assert.rejects(f.bridge.refresh(), { code: "ENOENT" });
  assert.equal(f.commands.length, 0);
  await rename(`${f.options.socketPath}.waiting`, f.options.socketPath);
  await f.bridge.refresh();
  assert(f.commands.some(command => command.startsWith("review-pr-attach ")));
});

test("linking a PR does not turn work mode into PR review", async t => {
  const f = await fixture(t);
  f.thread.linkedPullRequest = { url: "https://github.com/team/project/pull/7", repository: "team/project", number: 7 };
  await f.reopen();
  await f.bridge.tick();
  assert.equal(f.bridge.mode, "work");
  assert.equal(f.bridge.pullRequest, undefined);
  assert.equal(f.commands.filter(command => command.startsWith("review-pr-attach ")).length, 0);
  await f.batch(); await f.bridge.tick();
  assert.match(f.calls[0].message, /Why remove this check/u);
  assert.doesNotMatch(f.calls[0].message, /co-reviewer|GitHub suggestions/u);
});

test("startup fails on a stale attached PR before delivering review feedback", async t => {
  const f = await fixture(t);
  f.thread.linkedPullRequest = { url: "https://github.com/team/project/pull/7", repository: "team/project", number: 7 };
  await f.reopen("review");
  await f.batch();
  f.failAttach("the checked-out commit changed since the pull request was attached");
  await assert.rejects(f.bridge.refresh(), /checked-out commit changed/u);
  assert.equal(f.calls.length, 0);
  assert.deepEqual(await readdir(join(f.bridge.dir, "deliveries")), []);
  assert.deepEqual(await f.bridge.pending(), ["review.json"]);
});

test("work and review retain separate locks, sockets and pending feedback for the same thread", async t => {
  const f = await fixture(t);
  f.thread.linkedPullRequest = { url: "https://github.com/team/project/pull/7", repository: "team/project", number: 7 };
  await f.batch();
  const review = await ReviewBridge.open({ ...f.options, mode: "review", socketPath: undefined });
  t.after(() => review.close());
  assert.notEqual(review.dir, f.bridge.dir);
  assert.notEqual(review.socketPath, f.bridge.socketPath);
  assert.deepEqual(await review.pending(), []);
  assert.deepEqual(await f.bridge.pending(), ["review.json"]);
  await assert.rejects(ReviewBridge.open({ ...f.options, mode: "review", socketPath: undefined }), /already has an HX review open/u);
});

test("review mode uses the thread's only PR from T3's link list", async t => {
  const f = await fixture(t);
  f.linked.push("https://github.com/team/project/pull/7", "https://github.com/team/project/pull/8");
  await assert.rejects(ReviewBridge.open({ ...f.options, mode: "review" }), /several pull requests/u);
  f.linked.pop();
  assert.equal((await f.reopen("review")).pullRequest, "https://github.com/team/project/pull/7");
});

test("review mode requires a PR and never forwards a commit request", async t => {
  const f = await fixture(t);
  await assert.rejects(ReviewBridge.open({ ...f.options, mode: "review" }), /Link a pull request/u);
  f.thread.linkedPullRequest = { url: "https://github.com/team/project/pull/7", repository: "team/project", number: 7 };
  await f.reopen("review");
  await f.batch("commit.json", { kind: "commit-staged" });
  await f.bridge.tick();
  assert.equal(f.calls.length, 0);
  assert.deepEqual(await f.bridge.pending(), ["commit.json"]);
});


test("old-side selection feedback reaches exactly the bound thread with original baseline", async t => {
  const f = await fixture(t);
  await writeFile(join(f.repo, "file.txt"), "keep\nend\n");
  await f.batch(); await f.bridge.tick();
  assert.equal(f.calls.length, 1);
  assert.equal(f.calls[0].thread, "thread-1");
  assert.match(f.calls[0].message, /Why remove this check\?/u);
  assert.match(f.calls[0].message, /old check/u);
  assert(f.calls[0].message.includes(f.baseline));
  assert.deepEqual(await readdir(f.bridge.outbox), []);
  assert.equal((await readdir(join(f.bridge.dir, "receipts"))).length, 1);
});

test("failed delivery survives restart and retries the same frozen message/id", async t => {
  const f = await fixture(t);
  await f.batch(); f.fail(true); await f.bridge.tick();
  assert.equal(f.calls.length, 1);
  assert.deepEqual(await readdir(f.bridge.outbox), ["review.json"]);
  await writeFile(join(f.repo, "file.txt"), "changed after first dispatch\n");
  await f.reopen(); f.fail(false); await f.bridge.tick();
  assert.equal(f.calls.length, 2);
  assert.deepEqual(f.calls[1], f.calls[0]);
  assert.deepEqual(await readdir(f.bridge.outbox), []);
});

test("refuses to send when thread moves, branch changes, target differs, or a path escapes", async t => {
  const f = await fixture(t);
  await f.batch(); f.thread.cwd = f.dir;
  await f.bridge.tick(); assert.equal(f.calls.length, 0);
  f.thread.cwd = f.repo;
  await git(f.repo, ["checkout", "-b", "other"]);
  await f.bridge.tick(); assert.equal(f.calls.length, 0);
  await git(f.repo, ["checkout", "main"]);
  const path = join(f.bridge.outbox, "review.json");
  const batch = JSON.parse(await readFile(path, "utf8"));
  batch.target = "thread-elsewhere"; await writeFile(path, JSON.stringify(batch));
  await f.bridge.tick(); assert.equal(f.calls.length, 0);
  delete batch.target; batch.files[0].path = join(f.dir, "outside.txt");
  await writeFile(batch.files[0].path, "outside\n"); await writeFile(path, JSON.stringify(batch));
  await f.bridge.tick(); assert.equal(f.calls.length, 0);
  assert.deepEqual(await readdir(f.bridge.outbox), ["review.json"]);
});

test("saving alone sends nothing and another bridge cannot claim this thread", async t => {
  const f = await fixture(t);
  await writeFile(join(f.repo, "file.txt"), "user correction\n"); await f.bridge.tick();
  assert.equal(f.calls.length, 0);
  await assert.rejects(ReviewBridge.open(f.options), /already has an HX review open/u);
});

test("simultaneous stale-lock recovery cannot rename the new owner's lock", { timeout: 10_000 }, async t => {
  const f = await fixture(t);
  await f.bridge.close();
  const lock = join(f.bridge.dir, "bridge.lock");
  const token = "00000000-0000-4000-8000-000000000000";
  const stale = `${lock}.stale-${token}`;
  await mkdir(lock);
  await writeFile(join(lock, "owner.json"), JSON.stringify({ pid: 2147483647, token }));
  const originalKill = process.kill;
  t.mock.method(process, "kill", (pid: number, signal?: NodeJS.Signals | number) => {
    if (pid === 2147483647) throw Object.assign(new Error("dead test owner"), { code: "ESRCH" });
    return originalKill(pid, signal);
  });
  let bothRecovering!: () => void;
  const secondRecovery = new Promise<void>(done => { bothRecovering = done; });
  let ownerAcquired!: () => void;
  const newOwner = new Promise<void>(done => { ownerAcquired = done; });
  let recoveries = 0;
  const originalRename = fs.rename;
  const probe = t.mock.method(fs, "rename", async (...args: Parameters<typeof fs.rename>) => {
    if (args[0] === lock && args[1] === stale) {
      recoveries++;
      if (recoveries === 1) await secondRecovery;
      else if (recoveries === 2) { bothRecovering(); await newOwner; }
    }
    return originalRename(...args);
  });
  syncBuiltinESMExports();
  t.after(() => { probe.mock.restore(); syncBuiltinESMExports(); });
  const reopen = () => ReviewBridge.open(f.options).finally(ownerAcquired);
  const results = await Promise.allSettled([reopen(), reopen()]);
  const winners = results.filter((result): result is PromiseFulfilledResult<ReviewBridge> => result.status === "fulfilled");
  t.after(async () => { for (const result of winners) await result.value.close(); });
  assert.equal(recoveries, 2, "both contenders observed the same dead owner");
  assert.equal(winners.length, 1);
  const loser = results.find(result => result.status === "rejected") as PromiseRejectedResult;
  assert.match(loser.reason.message, /already has an HX review open/u);
  assert.equal(JSON.parse(await readFile(join(lock, "owner.json"), "utf8")).pid, process.pid);
  assert.equal(JSON.parse(await readFile(join(stale, "owner.json"), "utf8")).token, token);
  await winners[0].value.close();
  const reopened = await ReviewBridge.open(f.options);
  try { assert.equal((await stat(stale)).isDirectory(), true); }
  finally { await reopened.close(); }
});

test("a bridge allowed to wait takes the lock once its live predecessor closes", async t => {
  const f = await fixture(t);
  const originalKill = process.kill;
  let liveOwnerSeen!: () => void;
  const ownerChecked = new Promise<void>(done => { liveOwnerSeen = done; });
  t.mock.method(process, "kill", (pid: number, signal?: NodeJS.Signals | number) => {
    if (pid === process.pid && signal === 0) liveOwnerSeen();
    return originalKill(pid, signal);
  });
  const opening = ReviewBridge.open({ ...f.options, lockWait: 10_000 });
  await ownerChecked;
  await f.bridge.close();
  const successor = await opening;
  t.after(() => successor.close());
  assert.equal(successor.baseline, f.baseline);
  await assert.rejects(ReviewBridge.open({ ...f.options, lockWait: 1 }), /already has an HX review open/u);
});

test("legacy and incomplete lock owners require explicit cleanup", async t => {
  const f = await fixture(t);
  await f.bridge.close();
  const lock = join(f.bridge.dir, "bridge.lock");
  await writeFile(lock, JSON.stringify({ pid: 2147483647, token: "old-format" }));
  await assert.rejects(ReviewBridge.open(f.options), /legacy review lock.*remove that file/u);
  assert.equal((await stat(lock)).isFile(), true);
  await rm(lock);
  await mkdir(lock);
  await assert.rejects(ReviewBridge.open(f.options), /incomplete review lock.*removing that directory/u);
  assert.equal((await stat(lock)).isDirectory(), true);
});

test("opening or reopening a shared checkout never adopts another thread's branch", async t => {
  const f = await fixture(t);
  await f.bridge.close();
  await git(f.repo, ["checkout", "-b", "different"]);
  await assert.rejects(ReviewBridge.open(f.options), /belongs to branch main/u);
  f.thread.branch = "different";
  await assert.rejects(ReviewBridge.open(f.options), /different branch/u);
});

test("commit requests wait for idle and refuse changed staged content", async t => {
  const f = await fixture(t);
  await writeFile(join(f.repo, "file.txt"), "staged change\n"); await git(f.repo, ["add", "."]);
  await f.batch("review.json", { kind: "commit-staged" }); f.thread.status = "running";
  await f.bridge.tick(); assert.equal(f.calls.length, 0);
  await writeFile(join(f.repo, "file.txt"), "different staged change\n"); await git(f.repo, ["add", "."]);
  f.thread.status = "completed"; await f.bridge.tick(); assert.equal(f.calls.length, 0);
  assert.deepEqual(await readdir(f.bridge.outbox), ["review.json"]);
});

test("socket errors are not reported as successful attachment", async t => {
  const dir = await mkdtemp(join(tmpdir(), "hx-error-")); t.after(() => rm(dir, { recursive: true, force: true }));
  const path = join(dir, "hx.sock"); const server = createServer(socket => socket.on("data", () => socket.end("error unsaved buffer\n")));
  await new Promise<void>(done => server.listen(path, done));
  t.after(() => new Promise<void>(done => server.close(() => done())));
  await assert.rejects(sendCommands(path, ["review-baseline HEAD"]), /unsaved buffer/u);
});

test("command quoting and thread links preserve identity without command injection", () => {
  assert.equal(quoteCommand("It's 100% %{sh:echo bad}"), "'It''s 100% %{sh:echo bad}'");
  assert.throws(() => quoteCommand("hello\nquit"), /control/u);
  assert.equal(threadIdFromArgument("t3-thread://v1/thread-123"), "thread-123");
  assert.equal(threadIdFromArgument("thread:delegated-task:command%3A123"), "thread:delegated-task:command%3A123");
  assert.throws(() => threadIdFromArgument("x; quit"), /thread ID/u);
});

test("pending saved corrections keep their diff when the same tick observes run completion", async t => {
  const f = await fixture(t);
  const file = join(f.repo, "file.txt");
  await writeFile(file, "agent version\n");
  f.thread.latestRunId = "run-2";
  await f.bridge.tick();
  await writeFile(file, "user correction\n");
  await f.writeBatch("correction.json");
  f.thread.latestRunId = "run-3";
  await f.bridge.tick();
  assert.equal(f.calls.length, 1);
  const details = handoffDetails(f.calls[0].message);
  assert.equal(details.files[0].user_edits.against, "turn-snapshot");
  assert.match(details.files[0].user_edits.hunks.join(""), /-agent version\n\+user correction\n/u);
  assert.equal(await readFile(file, "utf8"), "user correction\n");
  assert.deepEqual(await readdir(f.bridge.outbox), []);
});

test("an explicit files-only correction already observed in a snapshot still acknowledges the authoritative saved version", async t => {
  const f = await fixture(t);
  const file = join(f.repo, "file.txt");
  await writeFile(file, "agent version\n");
  f.thread.latestRunId = "run-2";
  await f.bridge.tick();
  await writeFile(file, "user correction\n");
  f.thread.latestRunId = "run-3";
  await f.bridge.tick();
  assert.equal(f.calls.length, 0, "observing saved files alone should not send a message");
  await f.writeBatch("correction.json");
  await f.bridge.tick();
  assert.equal(f.calls.length, 1, "the explicit handoff must not disappear as an empty diff");
  assert.match(f.calls[0].message, /explicitly handed back these reviewed files/u);
  assert.match(f.calls[0].message, /user's intended version/u);
  assert.match(f.calls[0].message, /Preserve the saved code and do not restore an earlier agent version/u);
  assert.ok(f.calls[0].message.includes(file));
  assert.equal(await readFile(file, "utf8"), "user correction\n");
  assert.deepEqual(await readdir(f.bridge.outbox), []);
  assert.equal((await readdir(join(f.bridge.dir, "receipts"))).length, 1);
});

test("a review can hand back deletion of a file whose entire nested parent directory is gone", async t => {
  const f = await fixture(t);
  const directory = join(f.repo, "nested", "deeper");
  const file = join(directory, "removed.txt");
  await mkdir(directory, { recursive: true });
  await writeFile(file, "nested content\n");
  await git(f.repo, ["add", "--", "nested"]);
  await git(f.repo, ["commit", "-m", "nested baseline"]);
  const baseline = (await git(f.repo, ["rev-parse", "HEAD"])).trim();
  const blob = (await git(f.repo, ["rev-parse", "HEAD:nested/deeper/removed.txt"])).trim();
  await rm(join(f.repo, "nested"), { recursive: true });
  await f.writeBatch("deletion.json", { files: [{ path: file, baseline: { kind: "git-commit", commit_oid: baseline, blob_oid: blob } }] });
  await f.bridge.tick();
  assert.equal(f.calls.length, 1);
  const details = handoffDetails(f.calls[0].message);
  assert.equal(details.files[0].path, file);
  assert.match(details.files[0].user_edits.hunks.join(""), /-nested content\n/u);
  await assert.rejects(stat(file), { code: "ENOENT" });
  assert.deepEqual(await readdir(f.bridge.outbox), []);
});

test("an oversized saved file is kept pending without inferring deletion or sending feedback", async t => {
  const f = await fixture(t);
  const file = join(f.repo, "file.txt");
  const size = 8 * 1024 * 1024 + 1;
  await writeFile(file, Buffer.alloc(size, 0x78));
  await f.writeBatch("oversized.json");
  await f.bridge.tick();
  assert.equal(f.calls.length, 0);
  assert.deepEqual(await readdir(f.bridge.outbox), ["oversized.json"]);
  assert.deepEqual(await readdir(join(f.bridge.dir, "deliveries")), []);
  assert.equal((await stat(file)).size, size);
  assert.match(f.commands.join("\n"), /no deletion has been inferred/u);
});

test("renaming a bound thread refreshes the visible HX label without reconnecting its socket", async t => {
  const f = await fixture(t);
  await f.bridge.tick();
  const priorBindings = f.commands.filter(command => command.startsWith("review-handoff ")).length;
  f.thread.title = "Renamed parser review";
  await f.bridge.tick();
  const bindings = f.commands.filter(command => command.startsWith("review-handoff "));
  assert.equal(bindings.length, priorBindings + 1);
  assert.equal(bindings.at(-1), `review-handoff ${quoteCommand(f.bridge.outbox)} ${quoteCommand("T3: Renamed parser review")}`);
  assert.equal(f.bridge.title, f.thread.title);
  assert.equal(f.bridge.baseline, f.baseline);
  assert.equal(f.calls.length, 0);
});

test("an accepted commit with a lost response retries after HEAD advances using the same frozen queue request", async t => {
  const f = await fixture(t);
  const file = join(f.repo, "file.txt");
  await writeFile(file, "reviewed staged correction\n");
  await git(f.repo, ["add", "--", "file.txt"]);
  await f.writeBatch("commit.json", { request: { kind: "commit-staged", subject: "fix: reviewed correction" } });
  f.fail(true);
  await f.bridge.tick();
  assert.equal(f.calls.length, 1);
  assert.equal(f.calls[0].mode, "queue", "commit follow-ups must be queued as a separate turn");
  assert.match(f.calls[0].message, /fix: reviewed correction/u);
  assert.deepEqual(await readdir(f.bridge.outbox), ["commit.json"]);
  const deliveries = await readdir(join(f.bridge.dir, "deliveries"));
  const frozen = JSON.parse(await readFile(join(f.bridge.dir, "deliveries", deliveries[0]), "utf8"));
  assert.equal(frozen.attempted, true);
  assert.equal(frozen.head, f.baseline);
  await git(f.repo, ["commit", "-m", "accepted commit request"]);
  const committed = (await git(f.repo, ["rev-parse", "HEAD"])).trim();
  assert.notEqual(committed, frozen.head);
  f.thread.status = "running";
  f.thread.activeRunId = "run-2";
  f.thread.latestRunId = "run-2";
  await writeFile(file, "later agent work\n");
  await f.reopen();
  f.fail(false);
  await f.bridge.tick();
  assert.equal(f.calls.length, 2, "an attempted request must retry even while its accepted run is active");
  assert.deepEqual(f.calls[1], f.calls[0]);
  assert.equal(f.calls[1].mode, "queue");
  assert.equal((await git(f.repo, ["rev-parse", "HEAD"])).trim(), committed);
  assert.equal(await readFile(file, "utf8"), "later agent work\n");
  assert.deepEqual(await readdir(f.bridge.outbox), []);
  assert.equal((await readdir(join(f.bridge.dir, "receipts"))).length, 1);
});

test("a file growing beyond the size limit between inspection and handoff construction is never sent as a deletion", async t => {
  const f = await fixture(t);
  const file = join(f.repo, "file.txt");
  await f.writeBatch("growing.json");
  const originalStat = fs.stat;
  let reads = 0;
  const probe = t.mock.method(fs, "stat", async (...args: Parameters<typeof originalStat>) => {
    if (args[0] === file && ++reads === 2) await writeFile(file, Buffer.alloc(8 * 1024 * 1024 + 1, 0x78));
    return originalStat(...args);
  });
  syncBuiltinESMExports();
  t.after(() => { probe.mock.restore(); syncBuiltinESMExports(); });
  await f.bridge.tick();
  assert.ok(reads >= 2, "the file should change after its inspected read");
  assert.equal(f.calls.length, 0);
  assert.deepEqual(await readdir(f.bridge.outbox), ["growing.json"]);
  assert.deepEqual(await readdir(join(f.bridge.dir, "deliveries")), []);
  assert.match(f.commands.join("\n"), /No deletion has been inferred/u);
});

test("pending lists only batches in order and discard archives the original without delivering it", async t => {
  const f = await fixture(t);
  await f.writeBatch("z-last.json", { request: { kind: "message", text: "Keep this pending" } });
  await f.writeBatch("a-first.json", { request: { kind: "message", text: "Discard this request" } });
  await writeFile(join(f.bridge.outbox, "still-writing.tmp"), "incomplete");
  const original = await readFile(join(f.bridge.outbox, "a-first.json"), "utf8");
  assert.deepEqual(await f.bridge.pending(), ["a-first.json", "z-last.json"]);
  await assert.rejects(f.bridge.discard("../state.json"), /exact batch name/u);
  await assert.rejects(f.bridge.discard("still-writing.tmp"), /exact batch name/u);
  await f.bridge.discard("a-first.json");
  assert.deepEqual(await f.bridge.pending(), ["z-last.json"]);
  const archived = await readdir(join(f.bridge.dir, "discarded"));
  assert.equal(archived.length, 1);
  assert.ok(archived[0].endsWith("-a-first.json"));
  assert.equal(await readFile(join(f.bridge.dir, "discarded", archived[0]), "utf8"), original);
  assert.equal(f.calls.length, 0);
  await f.bridge.tick();
  assert.equal(f.calls.length, 1);
  assert.match(f.calls[0].message, /Keep this pending/u);
  assert.ok(!f.calls[0].message.includes("Discard this request"));
  assert.deepEqual(await f.bridge.pending(), []);
});

test("a pull request review waits for a linked PR, then asks the agent to publish it through T3", async t => {
  const f = await fixture(t);
  await writeFile(join(f.repo, "file.txt"), "keep\nnew check\nend\n");
  await f.writeBatch("pr.json", { request: { kind: "pr-review", summary: "Two nits" } });
  await f.bridge.tick();
  assert.equal(f.calls.length, 0);
  assert.deepEqual(await f.bridge.pending(), ["pr.json"]);
  assert.deepEqual(await readdir(join(f.bridge.dir, "deliveries")), []);
  assert(f.commands.some(command => command.startsWith("echo ") && command.includes("Link a pull request to this thread first")));

  f.linked.push("https://github.com/team/project/pull/7");
  await f.bridge.tick();
  assert.equal(f.calls.length, 1);
  assert.equal(f.calls[0].mode, "auto");
  const { message } = f.calls[0];
  assert(message.includes("Publish the review on https://github.com/team/project/pull/7 with T3's submit_pull_request_review MCP tool"));
  assert(message.includes(`headSha "${f.baseline}"`));
  assert(message.includes(`If the tool reports that the pull request's head is not ${f.baseline}, stop and tell the user`));
  assert.match(message, /suggestion fence/u);
  assert.match(message, /Only if that tool is unavailable, publish the same review with the host's CLI/u);
  const details = handoffDetails(message);
  assert.deepEqual(details.request, { kind: "pr-review", summary: "Two nits" });
  assert.equal(details.files[0].user_edits.against, "head");
  assert.match(details.files[0].user_edits.hunks.join(""), /^@@ -2 \+2 @@.*\n-old check\n\+new check\n$/u);
  assert.deepEqual(await f.bridge.pending(), []);
});

test("an empty pull request review is dropped without asking the agent to publish anything", async t => {
  const f = await fixture(t);
  f.linked.push("https://github.com/team/project/pull/7");
  await f.writeBatch("empty.json", { request: { kind: "pr-review" } });
  await f.bridge.tick();
  assert.equal(f.calls.length, 0);
  assert.deepEqual(await f.bridge.pending(), []);
  assert.deepEqual(await readdir(join(f.bridge.dir, "deliveries")), []);
  assert(f.commands.some(command => command.startsWith("echo ") && command.includes("nothing published")));
});

test("a pull request review on a thread with several linked PRs targets the checkout's branch", async t => {
  const f = await fixture(t);
  f.linked.push("https://github.com/team/project/pull/7", "https://github.com/team/project/pull/8");
  await writeFile(join(f.repo, "file.txt"), "keep\nnew check\nend\n");
  await f.writeBatch("pr.json", { request: { kind: "pr-review" } });
  await f.bridge.tick();
  assert.equal(f.calls.length, 1);
  assert(f.calls[0].message.includes("Publish the review on the one of https://github.com/team/project/pull/7, https://github.com/team/project/pull/8 whose head branch is main"));
});
