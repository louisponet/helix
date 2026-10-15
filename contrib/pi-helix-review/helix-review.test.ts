import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
	chmodSync,
	existsSync,
	mkdirSync,
	mkdtempSync,
	readdirSync,
	readFileSync,
	realpathSync,
	renameSync,
	rmSync,
	symlinkSync,
	unlinkSync,
	utimesSync,
	writeFileSync,
} from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { type TestContext, test } from "node:test";
import helixReview from "./index.ts";

const gitEnv = { ...process.env, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1" };

function git(cwd: string, ...args: string[]): string {
	return execFileSync("git", args, { cwd, env: gitEnv, encoding: "utf8" }).trim();
}

const indexBase = (blob_oid: string) => ({ kind: "git-index", blob_oid });
const commitBase = (commit_oid: string, blob_oid: string | null) => ({ kind: "git-commit", commit_oid, blob_oid });

/** Shutdowns of the sessions the running test started; its workspace waits for them before it goes. */
const shutdowns: (() => Promise<unknown>)[] = [];

function workspace(t: TestContext) {
	const root = mkdtempSync(join(tmpdir(), "pi-helix-review-"));
	// After hooks run in order: the sessions stop (and their pollers and ledger updates finish) first.
	t.after(async () => {
		for (const shutdown of shutdowns.splice(0)) await shutdown();
		rmSync(root, { recursive: true, force: true });
	});
	const repo = join(root, "repo");
	mkdirSync(repo);
	git(repo, "init", "-q");
	let batches = 0;
	return {
		root,
		repo,
		stage(name: string, content: string): string {
			writeFileSync(join(repo, name), content);
			git(repo, "add", "--", name);
			return git(repo, "rev-parse", `:${name}`);
		},
		commit(): string {
			git(repo, "-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-q", "-m", "c");
			return git(repo, "rev-parse", "HEAD");
		},
		batch(doc: unknown): string {
			const path = join(root, `batch-${++batches}.json`);
			writeFileSync(path, typeof doc === "string" || Buffer.isBuffer(doc) ? doc : JSON.stringify(doc, null, 2));
			return path;
		},
	};
}

// The public command against a recording Pi transport; filesystem and Git stay real.
function piSession(cwd: string, sessionId = "session-1") {
	const sent: { content: unknown; options: unknown }[] = [];
	const notes: { message: string; type?: string }[] = [];
	const statuses: (string | undefined)[] = [];
	const messages: { message: { customType: string; content: string; display: boolean; details: any }; options: unknown }[] = [];
	const handlers = new Map<string, ((event: unknown, ctx: unknown) => unknown)[]>();
	let handler: ((args: string, ctx: unknown) => Promise<void>) | undefined;
	helixReview({
		on(event: string, handler: (event: unknown, ctx: unknown) => unknown) {
			handlers.set(event, [...(handlers.get(event) ?? []), handler]);
		},
		sendMessage(message: (typeof messages)[number]["message"], options?: unknown) {
			messages.push({ message, options });
		},
		registerCommand(name: string, command: { handler: typeof handler }) {
			assert.equal(name, "helix-review");
			handler = command.handler;
		},
		sendUserMessage(content: unknown, options?: unknown) {
			sent.push({ content, options });
		},
	} as never);
	const ctx = {
		cwd,
		ui: {
			notify: (message: string, type?: string) => void notes.push({ message, type }),
			setStatus: (key: string, text: string | undefined) => {
				assert.equal(key, "helix-review");
				statuses.push(text);
			},
		},
		isIdle: () => true,
		hasPendingMessages: () => false,
		sessionManager: { getSessionId: () => sessionId },
	};
	const emit = async (type: string, event: object = {}) => {
		let result: unknown;
		for (const h of handlers.get(type) ?? []) result = (await h({ type, ...event }, ctx)) ?? result;
		return result;
	};
	shutdowns.push(() => emit("session_shutdown"));
	return { sent, notes, statuses, messages, ctx, emit, run: (args: string) => handler!(args, ctx) };
}

function simpleBatch(t: TestContext) {
	const w = workspace(t);
	const path = join(w.repo, "x.rs");
	const oid = w.stage("x.rs", "x\n");
	return w.batch({
		format: "helix-review-batch",
		version: 2,
		files: [{ path, baseline: indexBase(oid) }],
		comments: [],
	});
}

test("an idle import sends one plain user message quoting old and current review data", async (t) => {
	const w = workspace(t);
	const a = join(w.repo, "a.rs");
	const b = join(w.repo, "b.rs");
	const oidA = w.stage("a.rs", "line1\n// helpers\nfn removed() {\n}\n\nfn kept() {}\nlast\n");
	writeFileSync(a, "line1\n// helpers\n\nfn kept() {}\nfn added() {}\nlast\n");
	const oidB = w.stage("b.rs", "fn b() {}\n");
	writeFileSync(b, "fn b() { todo!() }\n");
	// Helix reviews a symlink against its target's index entry; the link's own entry differs.
	const link = join(w.repo, "link.rs");
	const oidTarget = w.stage("target.rs", "fn target() {}\n");
	symlinkSync("target.rs", link);
	git(w.repo, "add", "--", "link.rs");
	assert.notEqual(git(w.repo, "rev-parse", ":link.rs"), oidTarget);

	// Plain `git ls-files` would run this hook; the import must not.
	const marker = join(w.root, "hook-ran");
	const hook = join(w.root, "fsmonitor.sh");
	writeFileSync(hook, `#!/bin/sh\ntouch '${marker}'\nexit 1\n`);
	chmodSync(hook, 0o755);
	git(w.repo, "config", "core.fsmonitor", hook);

	const batchPath = w.batch({
		format: "helix-review-batch",
		version: 2,
		exporter: "future-field-top",
		files: [
			{ path: a, baseline: { ...indexBase(oidA), note: "future-field-baseline" }, extra: "future-field-file" },
			{ path: b, baseline: indexBase(oidB) },
			{ path: link, baseline: indexBase(oidTarget) },
		],
		comments: [
			{
				path: a,
				side: "old",
				baseline: indexBase(oidA),
				start_line: 3,
				end_line: 4,
				current_line: 3,
				selected_text: "fn removed() {\n}\n",
				context_before: "line1\n// helpers\n",
				context_after: "\nfn kept() {}\nlast\n",
				comment: "Why was this removed?",
				future: "future-field-comment",
			},
			{
				path: a,
				side: "current",
				baseline: indexBase(oidA),
				start_line: 5,
				end_line: 5,
				current_line: 5,
				selected_text: "fn added() {}",
				context_before: "// helpers\n\nfn kept() {}\n",
				context_after: "\nlast\n",
				comment: "Prefer ```rust\nfn added() -> u8``` here",
			},
			{
				path: link,
				side: "old",
				baseline: indexBase(oidTarget),
				start_line: 1,
				end_line: 1,
				current_line: 1,
				selected_text: "fn target() {}\n",
				context_before: "",
				context_after: "",
				comment: "Through the link",
			},
		],
	});
	const before = [a, b, batchPath].map((path) => readFileSync(path));

	const session = piSession(w.root);
	await session.run("batch-1.json");

	assert.equal(session.sent.length, 1);
	assert.equal(session.sent[0].options, undefined, "must not steer or queue");
	const message = session.sent[0].content as string;
	assert.equal(typeof message, "string");
	for (const expected of [
		JSON.stringify(batchPath),
		"Re-read every reviewed file from disk",
		"Line numbers are hints from comment time only",
		`### Comment 1: ${JSON.stringify(a)}, old side, against Git index blob ${oidA}`,
		"Old text from the Git index, as Helix displayed it",
		"index lines 3-4; it was removed or replaced just before current line 3",
		"Context before:\n```\nline1\n// helpers\n```",
		"Selected text:\n```\nfn removed() {\n}\n```",
		"Context after:\n```\n\nfn kept() {}\nlast\n```",
		"Reviewer comment:\n```\nWhy was this removed?\n```",
		`### Comment 2: ${JSON.stringify(a)}, current side, against Git index blob ${oidA}`,
		"Comment-time hint: line 5. In the saved file now: selection and context found at line 5.",
		"Selected text:\n```\nfn added() {}\n```",
		"Reviewer comment:\n````\nPrefer ```rust\nfn added() -> u8``` here\n````",
		`- ${JSON.stringify(a)}: exported baseline Git index blob ${oidA}, 2 comments`,
		`- ${JSON.stringify(b)}: exported baseline Git index blob ${oidB}, no comments (saved edits only)`,
		`### Comment 3: ${JSON.stringify(link)}, old side, against Git index blob ${oidTarget}`,
		`- ${JSON.stringify(link)}: exported baseline Git index blob ${oidTarget}, 1 comment`,
	]) {
		assert.ok(message.includes(expected), `message lacks ${JSON.stringify(expected)}:\n${message}`);
	}
	assert.ok(!message.includes("## Warnings"), message);
	assert.ok(!message.includes("future-field"), message);
	assert.ok(!existsSync(marker), "the repository's fsmonitor hook ran");
	assert.deepEqual(
		[a, b, batchPath].map((path) => readFileSync(path)),
		before,
		"batch or source files were modified",
	);
	assert.equal(session.notes.at(-1)?.type, "info");
});

test("a batch with reviewed files but no comments is a saved-edit-only handoff", async (t) => {
	const w = workspace(t);
	const c = join(w.repo, "c.rs");
	const oid = w.stage("c.rs", "a\n");
	writeFileSync(c, "b\n");
	const batchPath = w.batch({ format: "helix-review-batch", version: 2, files: [{ path: c, baseline: indexBase(oid) }], comments: [] });

	const session = piSession(w.root);
	await session.run(batchPath);

	assert.equal(session.sent.length, 1);
	assert.equal(session.sent[0].options, undefined);
	const message = session.sent[0].content as string;
	assert.ok(message.includes("There are no inline comments: the saved edits are the whole review."), message);
	assert.ok(message.includes(`- ${JSON.stringify(c)}: exported baseline Git index blob ${oid}, no comments (saved edits only)`), message);
	assert.ok(message.includes("Re-read every reviewed file from disk"), message);
	assert.ok(!message.includes("## Comments") && !message.includes("## Warnings"), message);
});

test("a busy pi gets nothing, even when it becomes busy during the import", async (t) => {
	const batchPath = simpleBatch(t);

	const busy = piSession("/");
	busy.ctx.isIdle = () => false;
	await busy.run(batchPath);
	assert.equal(busy.sent.length, 0);
	assert.match(busy.notes.at(-1)!.message, /pi is busy; nothing was sent/);

	const queued = piSession("/");
	queued.ctx.hasPendingMessages = () => true;
	await queued.run(batchPath);
	assert.equal(queued.sent.length, 0);
	assert.match(queued.notes.at(-1)!.message, /queued messages; nothing was sent/);

	const turnStarted = piSession("/");
	let checks = 0;
	turnStarted.ctx.isIdle = () => checks++ === 0;
	await turnStarted.run(batchPath);
	assert.equal(turnStarted.sent.length, 0);
	assert.match(turnStarted.notes.at(-1)!.message, /pi is busy during the import; nothing was sent/);

	const twice = piSession("/");
	await Promise.all([twice.run(batchPath), twice.run(batchPath)]);
	assert.equal(twice.sent.length, 1, "overlapping imports must send one message");
	assert.equal(twice.sent[0].options, undefined);
	assert.ok(twice.notes.some((note) => /another \/helix-review import is running/.test(note.message)));
});

test("a session switch or reload during the import sends nothing", async (t) => {
	const batchPath = simpleBatch(t);

	const switched = piSession("/");
	const ids = ["session-1", "session-2"];
	switched.ctx.sessionManager.getSessionId = () => ids.shift() ?? "session-2";
	await switched.run(batchPath);
	assert.equal(switched.sent.length, 0);
	assert.match(switched.notes.at(-1)!.message, /the session changed during the import; nothing was sent/);

	// A replaced runtime makes every use of the captured ctx throw.
	const replaced = piSession("/");
	let stale = false;
	replaced.ctx.sessionManager.getSessionId = () => {
		if (stale) throw new Error("This extension ctx is stale");
		stale = true;
		return "session-1";
	};
	replaced.ctx.ui.notify = () => {
		throw new Error("This extension ctx is stale");
	};
	await assert.rejects(replaced.run(batchPath), /session was replaced or reloaded during the import; nothing was sent/);
	assert.equal(replaced.sent.length, 0);
});

test("malformed or unsupported batches are rejected without sending", async (t) => {
	const w = workspace(t);
	const path = join(w.repo, "m.rs");
	const oid = w.stage("m.rs", "m\n");
	const comment = {
		path,
		side: "current",
		baseline: indexBase(oid),
		start_line: 1,
		end_line: 1,
		current_line: 1,
		selected_text: "m",
		context_before: "",
		context_after: "\n",
		comment: "c",
	};
	const valid = { format: "helix-review-batch", version: 2, files: [{ path, baseline: indexBase(oid) }], comments: [comment] };
	const v1 = { format: "helix-review-batch", version: 1, baseline: "git-index", files: [{ path, index_oid: oid }], comments: [] };
	const v1Comment = { ...comment, baseline: undefined, index_oid: oid };
	const cases: [string, unknown, RegExp][] = [
		["not JSON", "{", /not valid JSON/],
		["invalid UTF-8", Buffer.from([0x7b, 0xff, 0x7d]), /not valid UTF-8/],
		["not an object", "[]", /batch must be an object/],
		["format", { ...valid, format: "other" }, /unsupported format "other"/],
		["version 1", v1, /unsupported version 1 \(supported: 2; re-export the review with an updated Helix\)/],
		["version", { ...valid, version: 3 }, /unsupported version 3/],
		["files", { ...valid, files: undefined }, /files must be an array/],
		["relative path", { ...valid, files: [{ path: "m.rs", baseline: indexBase(oid) }] }, /files\[0\]\.path must be an absolute path/],
		["v1 file", { ...valid, files: [{ path, index_oid: oid }] }, /files\[0\]\.baseline must be an object/],
		["v1 comment", { ...valid, comments: [v1Comment] }, /comments\[0\]\.baseline must be an object/],
		["baseline kind", { ...valid, files: [{ path, baseline: { kind: "head", blob_oid: oid } }] }, /files\[0\]\.baseline\.kind must be "git-index" or "git-commit"/],
		["object id", { ...valid, files: [{ path, baseline: indexBase("HEAD") }] }, /files\[0\]\.baseline\.blob_oid must be a hex Git object id/],
		["index absence", { ...valid, files: [{ path, baseline: { kind: "git-index", blob_oid: null } }] }, /files\[0\]\.baseline\.blob_oid must be a string/],
		["short commit", { ...valid, files: [{ path, baseline: commitBase(oid.slice(0, 12), oid) }] }, /files\[0\]\.baseline\.commit_oid must be a hex Git object id/],
		["commit blob", { ...valid, comments: [{ ...comment, baseline: { kind: "git-commit", commit_oid: oid } }] }, /comments\[0\]\.baseline\.blob_oid must be a string/],
		["old absent file", { ...valid, comments: [{ ...comment, side: "old", baseline: commitBase(oid, null) }] }, /cannot select old text from an absent file/],
		["duplicate file", { ...valid, files: [valid.files[0], valid.files[0]] }, /lists .* more than once/],
		["side", { ...valid, comments: [{ ...comment, side: "both" }] }, /comments\[0\]\.side must be "old" or "current"/],
		["line", { ...valid, comments: [{ ...comment, start_line: 0 }] }, /comments\[0\]\.start_line must be a 1-based line number/],
		["range", { ...valid, comments: [{ ...comment, start_line: 2 }] }, /comments\[0\]\.end_line is before start_line/],
		["text", { ...valid, comments: [{ ...comment, selected_text: 1 }] }, /comments\[0\]\.selected_text must be a string/],
		["empty", { ...valid, files: [], comments: [] }, /no files or comments/],
		["oversized", " ".repeat(4 * 1024 * 1024 + 1), /larger than 4 MiB/],
	];
	for (const [name, doc, expected] of cases) {
		const batchPath = w.batch(doc);
		const session = piSession(w.root);
		await session.run(batchPath);
		assert.equal(session.sent.length, 0, name);
		assert.equal(session.notes.at(-1)?.type, "error", name);
		assert.match(session.notes.at(-1)!.message, expected, name);
		assert.match(session.notes.at(-1)!.message, /Nothing was sent/, name);
		assert.ok(existsSync(batchPath), name);
	}

	const missing = piSession(w.root);
	await missing.run(join(w.root, "missing.json"));
	assert.equal(missing.sent.length, 0);
	assert.match(missing.notes.at(-1)!.message, /cannot read it/);
});

test("stale snapshots are reported, not remapped onto new lines", async (t) => {
	const w = workspace(t);
	const a = join(w.repo, "a.rs");
	const b = join(w.repo, "b.rs");
	const unlisted = join(w.repo, "c.rs");
	const outside = join(w.root, "outside.txt");
	const oidA = w.stage("a.rs", "one\ntwo\nthree\n");
	writeFileSync(a, "one\nTWO\nthree\n");
	const oidB = w.stage("b.rs", "b\n");
	const oidC = w.stage("c.rs", "c\n");
	writeFileSync(outside, "outside\n");
	const current = (selected_text: string, start_line: number, context_before: string, context_after: string) => ({
		path: a,
		side: "current",
		baseline: indexBase(oidA),
		start_line,
		end_line: start_line,
		current_line: start_line,
		selected_text,
		context_before,
		context_after,
		comment: `about ${selected_text}`,
	});
	const batchPath = w.batch({
		format: "helix-review-batch",
		version: 2,
		files: [
			{ path: a, baseline: indexBase(oidA) },
			{ path: b, baseline: indexBase(oidB) },
			{ path: outside, baseline: indexBase(oidB) },
		],
		comments: [
			{
				path: a,
				side: "old",
				baseline: indexBase(oidA),
				start_line: 2,
				end_line: 2,
				current_line: 2,
				selected_text: "two\n",
				context_before: "one\n",
				context_after: "three\n",
				comment: "keep two",
			},
			current("TWO", 2, "one\n", "\nthree\n"),
			current("deleted later", 3, "TWO\n", "\n"),
			current("three", 3, "TWO\n", "\nEND\n"),
			{ ...current("c", 1, "", "\n"), path: unlisted, baseline: indexBase(oidC) },
		],
	});

	// After export: the index moves, the saved file gains a line above the comments, and b is deleted.
	const oidA2 = w.stage("a.rs", "one\n2\nthree\n");
	writeFileSync(a, "zero\none\nTWO\nthree\nfour\n");
	unlinkSync(b);

	const session = piSession(w.root);
	await session.run(batchPath);

	assert.equal(session.sent.length, 1);
	const message = session.sent[0].content as string;
	const warnings = message.slice(message.indexOf("## Warnings"), message.indexOf("## Reviewed files"));
	for (const expected of [
		`${JSON.stringify(a)}: the Git index changed after export (exported baseline ${oidA}, index now ${oidA2})`,
		`Comment 1: the Git index for ${JSON.stringify(a)} changed since the comment (comment baseline ${oidA}, index now ${oidA2})`,
		`Comment 3 (${JSON.stringify(a)}): the selected text was not found in the saved file read as UTF-8`,
		`Comment 4 (${JSON.stringify(a)}): the selection and its context were not found together in the saved file read as UTF-8; the selected text alone appears at line 4`,
		`Comment 5: ${JSON.stringify(unlisted)} is not in the batch's list of reviewed files`,
		`${JSON.stringify(b)} is missing on disk.`,
		`${JSON.stringify(outside)} could not be checked against the Git index (`,
	]) {
		assert.ok(warnings.includes(expected), `warnings lack ${JSON.stringify(expected)}:\n${warnings}`);
	}
	assert.ok(!warnings.includes("Comment 2"), "a moved but intact selection is not stale");
	assert.ok(message.includes("Comment-time hint: line 2. In the saved file now: selection and context found at line 3."), message);
	assert.ok(message.includes("Selected text:\n```\ntwo\n```"), "old-side snapshot must stay quoted");
	assert.equal(session.notes.at(-1)?.type, "warning");
});

test("mixed baselines are labelled per entry, and a pinned commit stays valid after HEAD and the index move", async (t) => {
	const w = workspace(t);
	mkdirSync(join(w.repo, "sub"));
	const a = join(w.repo, "sub", "a.rs");
	const b = join(w.repo, "b.rs");
	const added = join(w.repo, "added.rs");
	w.stage("sub/a.rs", "one\n");
	const oidB = w.stage("b.rs", "b\n");
	const c0 = w.commit();
	w.stage("sub/a.rs", "one\ntwo\n");
	const c1 = w.commit();
	const blobA0 = git(w.repo, "rev-parse", `${c0}:sub/a.rs`);
	const blobA1 = git(w.repo, "rev-parse", `${c1}:sub/a.rs`);
	// An index-era comment survives a switch to the commit baseline with its own metadata.
	const oidIndex = w.stage("sub/a.rs", "one\ntwo\nstaged\n");
	writeFileSync(a, "one\nTWO\nstaged\n");
	writeFileSync(b, "b2\n");
	writeFileSync(added, "fn added() {}\n");
	const old = (baseline: unknown, comment: string) => ({
		path: a,
		side: "old",
		baseline,
		start_line: 2,
		end_line: 2,
		current_line: 2,
		selected_text: "two\n",
		context_before: "one\n",
		context_after: "",
		comment,
	});
	const batchPath = w.batch({
		format: "helix-review-batch",
		version: 2,
		files: [
			{ path: a, baseline: commitBase(c1, blobA1) },
			{ path: b, baseline: indexBase(oidB) },
			{ path: added, baseline: commitBase(c1, null) },
		],
		comments: [
			old(commitBase(c1, blobA1), "why uppercase?"),
			{
				path: a,
				side: "current",
				baseline: commitBase(c0, blobA0),
				start_line: 2,
				end_line: 2,
				current_line: 2,
				selected_text: "TWO",
				context_before: "one\n",
				context_after: "\nstaged\n",
				comment: "from the older commit review",
			},
			old(indexBase(oidIndex), "from the index review"),
			{
				path: added,
				side: "current",
				baseline: commitBase(c1, null),
				start_line: 1,
				end_line: 1,
				current_line: 1,
				selected_text: "fn added() {}",
				context_before: "",
				context_after: "\n",
				comment: "new file",
			},
		],
	});

	// After export a newer commit moves HEAD past c1 and the index moves again; neither makes c1 stale.
	w.stage("sub/a.rs", "one\nnewer\n");
	w.commit();
	const oidNow = w.stage("sub/a.rs", "one\nnewest\n");
	writeFileSync(a, "one\nTWO\nstaged\n");

	const session = piSession(w.root);
	await session.run(batchPath);

	assert.equal(session.sent.length, 1);
	assert.equal(session.sent[0].options, undefined);
	const message = session.sent[0].content as string;
	const q = JSON.stringify;
	for (const expected of [
		"(helix-review-batch v2)",
		`- ${q(a)}: exported baseline Git commit ${c1} (blob ${blobA1}), 3 comments`,
		`- ${q(b)}: exported baseline Git index blob ${oidB}, no comments (saved edits only)`,
		`- ${q(added)}: exported baseline Git commit ${c1} (file absent there, so all current text is added), 1 comment`,
		`### Comment 1: ${q(a)}, old side, against Git commit ${c1} (blob ${blobA1})`,
		"Old text from that commit (historical, not the current index or HEAD), as Helix displayed it after worktree conversions such as line endings. Comment-time hints: commit line 2; it was removed or replaced just before current line 2.",
		`### Comment 2: ${q(a)}, current side, against Git commit ${c0} (blob ${blobA0})`,
		"Comment-time hint: line 2. In the saved file now: selection and context found at line 2.",
		`### Comment 3: ${q(a)}, old side, against Git index blob ${oidIndex}`,
		`### Comment 4: ${q(added)}, current side, against Git commit ${c1} (file absent there, so all current text is added)`,
	]) {
		assert.ok(message.includes(expected), `message lacks ${q(expected)}:\n${message}`);
	}
	const warnings = message.slice(message.indexOf("## Warnings"), message.indexOf("## Reviewed files"));
	assert.deepEqual(
		warnings.split("\n").filter((line) => line.startsWith("- ")),
		[
			`- Comment 3: the Git index for ${q(a)} changed since the comment (comment baseline ${oidIndex}, index now ${oidNow}); its quoted old text is from the comment baseline.`,
		],
	);
	assert.equal(session.notes.at(-1)?.type, "warning");
});

test("commit baselines that do not match the commit, or name an unavailable commit, are warned about", async (t) => {
	const w = workspace(t);
	const x = join(w.repo, "x.rs");
	const y = join(w.repo, "y.rs");
	const z = join(w.repo, "z.rs");
	w.stage("x.rs", "x\n");
	w.stage("y.rs", "y\n");
	const directory = join(w.repo, "was-directory");
	mkdirSync(directory);
	w.stage("was-directory/child.txt", "child\n");
	const c1 = w.commit();
	const blobX = git(w.repo, "rev-parse", `${c1}:x.rs`);
	const blobY = git(w.repo, "rev-parse", `${c1}:y.rs`);
	const tree = git(w.repo, "rev-parse", `${c1}^{tree}`);
	const directoryOid = git(w.repo, "rev-parse", `${c1}:was-directory`);
	rmSync(directory, { recursive: true });
	writeFileSync(directory, "now a regular file\n");
	const unknown = "0123456789abcdef0123456789abcdef01234567";
	writeFileSync(z, "z\n");
	const note = (path: string, baseline: unknown) => ({
		path,
		side: "current",
		baseline,
		start_line: 1,
		end_line: 1,
		current_line: 1,
		selected_text: readFileSync(path, "utf8").trimEnd(),
		context_before: "",
		context_after: "\n",
		comment: "c",
	});
	const batchPath = w.batch({
		format: "helix-review-batch",
		version: 2,
		files: [
			{ path: x, baseline: commitBase(c1, blobY) },
			{ path: y, baseline: commitBase(c1, null) },
			{ path: z, baseline: commitBase(c1, blobX) },
			{ path: directory, baseline: commitBase(c1, directoryOid) },
		],
		comments: [note(x, commitBase(c1, blobY)), note(x, commitBase(unknown, blobX)), note(z, commitBase(tree, null))],
	});

	const session = piSession(w.root);
	await session.run(batchPath);

	assert.equal(session.sent.length, 1);
	const message = session.sent[0].content as string;
	const warnings = message.slice(message.indexOf("## Warnings"), message.indexOf("## Reviewed files"));
	const q = JSON.stringify;
	const mismatchX = `${q(x)}: Git commit ${c1} has object ${blobX} at this path, but the batch says blob ${blobY}.`;
	for (const expected of [
		mismatchX,
		`${q(y)}: Git commit ${c1} has object ${blobY} at this path, but the batch says the file is absent there.`,
		`${q(z)}: Git commit ${c1} has no entry at this path, but the batch says blob ${blobX}.`,
		`${q(x)} could not be checked against Git commit ${unknown} (`,
		`${q(z)} could not be checked against Git commit ${tree} (`,
		`${q(directory)} has a non-regular file entry in Git commit ${c1}; the historical baseline is unverified`,
	]) {
		assert.ok(warnings.includes(expected), `warnings lack ${q(expected)}:\n${warnings}`);
	}
	assert.equal(warnings.split(mismatchX).length, 2, "a shared baseline is reported once");
	assert.ok(!warnings.includes("Git index"), warnings);
	assert.equal(session.notes.at(-1)?.type, "warning");
});

function fakeHelix(t: TestContext, path: string) {
	const lines: string[] = [];
	const server = createServer((socket) => {
		let pending = "";
		socket.on("data", (chunk) => {
			const parts = (pending + chunk.toString()).split("\n");
			pending = parts.pop()!;
			for (const line of parts) {
				lines.push(line);
				socket.write(line.startsWith("review-") ? "ok\n" : `error unknown command ${line}\n`);
			}
		});
	});
	t.after(() => server.close());
	const quit = () => new Promise<void>((done) => server.close(() => done()));
	return new Promise<{ lines: string[]; quit: typeof quit }>((done) => server.listen(path, () => done({ lines, quit })));
}

function withEnv(t: TestContext, vars: Record<string, string | undefined>) {
	const saved = Object.fromEntries(Object.keys(vars).map((key) => [key, process.env[key]]));
	const apply = (values: Record<string, string | undefined>) => {
		for (const [key, value] of Object.entries(values)) {
			if (value === undefined) delete process.env[key];
			else process.env[key] = value;
		}
	};
	apply(vars);
	t.after(() => apply(saved));
}

async function until(condition: () => boolean, what: string) {
	for (let i = 0; i < 200 && !condition(); i++) await new Promise((wake) => setTimeout(wake, 25));
	assert.ok(condition(), `timed out waiting for ${what}`);
}

test("hook mode drives Helix over its socket and turns a saved review into one steering handoff", async (t) => {
	const w = workspace(t);
	const runtime = join(w.root, "run");
	mkdirSync(join(runtime, "helix"), { recursive: true });
	withEnv(t, { HELIX_COMMAND_SOCKET: undefined, XDG_RUNTIME_DIR: runtime });
	const repo = realpathSync(w.repo);
	const id = createHash("sha256").update(repo).digest("hex").slice(0, 32);
	const { lines } = await fakeHelix(t, join(runtime, "helix", `${id}.sock`));

	const file = join(repo, "a.rs");
	w.stage("a.rs", "fn a() {}\n");
	const head = w.commit();
	const outbox = join(repo, ".pi", "helix-review", "outbox");
	const state = () => JSON.parse(readFileSync(join(repo, ".pi", "helix-review", "state", "session-1.json"), "utf8"));

	const session = piSession(join(repo));
	t.after(() => session.emit("session_shutdown"));
	await session.emit("session_start", { reason: "startup" });
	assert.ok(existsSync(outbox));
	assert.deepEqual(lines.splice(0), [`review-handoff ${outbox}`, "echo pi hook attached as pi-1"], "no baseline to restore yet");
	assert.ok(existsSync(join(repo, ".pi", "helix-review", ".gitignore")));
	const promptGuidelines: string[] = [];
	await session.emit("before_agent_start", { prompt: "p", systemPrompt: "base", systemPromptOptions: { promptGuidelines } });
	assert.equal(promptGuidelines.length, 1);
	assert.match(promptGuidelines[0], /^## Helix review handoff\n/);
	assert.doesNotMatch(promptGuidelines[0], /AGENT:|by saving/);
	assert.match(promptGuidelines[0], /explicit requests.*steering message.*review_comments.*turn snapshot.*keep it undone.*integrated or declined.*never run git commit or git push on your own.*commit-staged handoff.*exactly the index/s);

	// The fake Helix rejects `echo`, which must not fail the hook.
	const started = "echo pi-1: Agent turn started";
	await session.emit("agent_start");
	assert.deepEqual(lines, [`review-handoff ${outbox}`, `review-baseline ${head}`, started]);
	assert.deepEqual(state().baseline, { commit_oid: head });

	writeFileSync(file, "fn a() {}\n// AGENT: old request\nfn b() {}\n");
	for (const toolName of ["edit", "write", "bash", "read"]) await session.emit("tool_result", { toolName, input: {}, content: [], isError: false });
	await until(() => lines.length === 4, "the coalesced refresh");
	await new Promise((wake) => setTimeout(wake, 300));
	assert.deepEqual(lines.slice(3), [`review-baseline ${head}`]);

	writeFileSync(join(repo, "new.rs"), "fn new() {}\n");
	// pi's own state is neither snapshotted nor counted as a changed file.
	const piState = join(repo, ".pi", "fabric", "state.json");
	mkdirSync(dirname(piState), { recursive: true });
	writeFileSync(piState, "{}\n");
	await session.emit("agent_end", { messages: [] });
	assert.deepEqual(lines.slice(4), [
		`review-baseline ${head}`,
		`echo pi-1: Agent turn done, 2 files changed since ${head.slice(0, 8)} (:review-files)`,
	]);
	const snapshots = state().snapshots;
	assert.deepEqual(Object.keys(snapshots).sort(), [file, join(repo, "new.rs")]);
	assert.equal(snapshots[file], git(repo, "hash-object", file));

	await session.emit("agent_start");
	assert.deepEqual(lines.slice(6), [`review-handoff ${outbox}`, `review-baseline ${head}`, started], "no commit, so the baseline stays");

	writeFileSync(file, "fn a() {}\n// AGENT: old request\nfn b() -> u8 { 1 }\n// AGENT: rename b\n");
	const blob = git(repo, "rev-parse", `${head}:a.rs`);
	const batch = join(outbox, "1-1.json");
	writeFileSync(
		`${batch}.tmp`,
		JSON.stringify({
			format: "helix-review-batch",
			version: 2,
			// pi's own state in a batch is skipped rather than diffed.
			files: [
				{ path: file, baseline: commitBase(head, blob) },
				{ path: piState, baseline: commitBase(head, null) },
			],
			comments: [
				{
					path: file,
					side: "current",
					baseline: commitBase(head, blob),
					start_line: 1,
					end_line: 1,
					current_line: 1,
					selected_text: "fn a() {}",
					context_before: "",
					context_after: "\n",
					comment: "document a",
				},
			],
		}),
	);
	renameSync(`${batch}.tmp`, batch);
	await until(() => !existsSync(batch), "the batch to be consumed");

	assert.equal(session.messages.length, 1);
	const { message, options } = session.messages[0];
	assert.deepEqual(options, { triggerTurn: true, deliverAs: "steer" });
	assert.equal(message.customType, "helix-review:handoff");
	assert.equal(message.display, true);
	assert.equal(message.details.format, "helix-review-handoff");
	assert.equal(message.details.version, 1);
	assert.equal(message.details.baseline, head);
	assert.equal(message.details.files.length, 1);
	const handoff = message.details.files[0];
	assert.equal(handoff.path, file);
	assert.equal(handoff.user_edits.against, "turn-snapshot");
	assert.equal(handoff.user_edits.hunks.length, 1);
	assert.equal(handoff.user_edits.truncated, false);
	assert.match(handoff.user_edits.hunks[0], /^@@ .* @@\n.*-fn b\(\) \{\}\n\+fn b\(\) -> u8 \{ 1 \}\n\+\/\/ AGENT: rename b\n$/s);
	// AGENT: lines are only text now.
	assert.equal(handoff.agent_comments, undefined);
	assert.deepEqual(handoff.review_comments.map((c: { comment: string }) => c.comment), ["document a"]);
	assert.ok(message.content.includes(JSON.stringify(message.details, null, 2)), message.content);
	assert.match(message.content, /never revert them/);
	await until(() => lines.length === 10, "the receipt echo");
	assert.equal(lines[9], "echo pi-1 received your review: 1 file with edits, 1 review comment");

	// An unchanged file without comments is nothing to do, and the reviewer is told so.
	const empty = join(outbox, "1-2.json");
	writeFileSync(
		`${empty}.tmp`,
		JSON.stringify({
			format: "helix-review-batch",
			version: 2,
			files: [{ path: join(repo, "new.rs"), baseline: commitBase(head, null) }],
			comments: [],
		}),
	);
	renameSync(`${empty}.tmp`, empty);
	await until(() => lines.length === 11, "the empty receipt echo");
	assert.equal(lines[10], "echo pi-1 received your review: nothing to do");

	assert.equal(session.messages.length, 1);

	// A large edit embeds whole hunks up to 200 lines and summarizes the rest.
	const saved = readFileSync(file, "utf8");
	writeFileSync(file, saved + Array.from({ length: 300 }, (_, i) => `// line ${i}\n`).join(""));
	const large = join(outbox, "1-3.json");
	writeFileSync(
		`${large}.tmp`,
		JSON.stringify({ format: "helix-review-batch", version: 2, files: [{ path: file, baseline: commitBase(head, blob) }], comments: [] }),
	);
	renameSync(`${large}.tmp`, large);
	await until(() => lines.length === 12, "the large receipt echo");
	assert.equal(session.messages.length, 2);
	const truncated = session.messages[1].message.details.files[0].user_edits;
	assert.equal(truncated.truncated, true);
	assert.equal(truncated.hunks.length, 1);
	assert.match(truncated.hunks[0], /^\.\.\. 1 more hunk \(30\d lines\) omitted; read the file$/);
	writeFileSync(file, saved);

	// A request makes an otherwise empty batch a handoff, with the instruction spelled out.
	const requested = (name: string, request: unknown) => {
		const path = join(outbox, name);
		writeFileSync(`${path}.tmp`, JSON.stringify({ format: "helix-review-batch", version: 2, files: [], comments: [], request }));
		renameSync(`${path}.tmp`, path);
	};
	requested("1-4.json", { kind: "commit-staged", subject: "fix: capitalize b" });
	await until(() => lines.length === 13, "the commit request receipt");
	assert.equal(lines[12], "echo pi-1 received your review: commit request");
	assert.equal(session.messages.length, 3);
	const commit = session.messages[2].message;
	assert.deepEqual(commit.details.files, []);
	assert.deepEqual(commit.details.request, { kind: "commit-staged", subject: "fix: capitalize b" });
	assert.match(commit.content, /User request:\nCommit exactly what is staged in the git index: run `git commit` without `-a`/);
	assert.match(commit.content, /Use this subject: "fix: capitalize b"\..*leave unstaged changes alone, and report the commit hash\./);
	requested("1-5.json", { kind: "message", text: "Please also update the changelog." });
	await until(() => lines.length === 14, "the message receipt");
	assert.equal(lines[13], "echo pi-1 received your review: message");
	const instruction = session.messages[3].message;
	assert.deepEqual(instruction.details.request, { kind: "message", text: "Please also update the changelog." });
	assert.match(instruction.content, /User request:\nInstruction:\n```\nPlease also update the changelog\.\n```$/);

	w.stage("a.rs", "fn a() {}\n// AGENT: old request\nfn b() {}\n");
	const next = w.commit();
	await session.emit("agent_start");
	assert.deepEqual(lines.slice(14), [`review-handoff ${outbox}`, `review-baseline ${next}`, started], "a commit moves the baseline to HEAD");

	const restarted = piSession(repo);
	t.after(() => restarted.emit("session_shutdown"));
	await restarted.emit("session_start", { reason: "startup" });
	assert.deepEqual(
		lines.slice(17),
		[`review-handoff ${outbox}`, `review-baseline ${next}`, `echo pi hook attached as pi-1, baseline ${next.slice(0, 8)}`],
		"session_start restores the baseline and says the link is live",
	);

	// The commit made after the commit request (1-4) fulfils it: that turn's end forces it and
	// says so, with nothing left changed.
	writeFileSync(file, "fn a() {}\n// AGENT: old request\nfn b() {}\n");
	unlinkSync(join(repo, "new.rs"));
	await restarted.emit("agent_start");
	await restarted.emit("agent_end", { messages: [] });
	assert.deepEqual(lines.slice(20), [
		`review-handoff ${outbox}`,
		`review-baseline ${next}`,
		started,
		`review-baseline ${next} force`,
		`review-baseline ${next}`,
		`echo pi-1: Agent turn done, committed ${next.slice(0, 8)}, 0 files still changed`,
	]);
	// Without a request, a turn that leaves nothing changed since its baseline says so.
	await restarted.emit("agent_start");
	await restarted.emit("agent_end", { messages: [] });
	assert.deepEqual(lines.slice(26), [
		`review-handoff ${outbox}`,
		`review-baseline ${next}`,
		started,
		`review-baseline ${next}`,
		`echo pi-1: Agent turn done, no file changes since ${next.slice(0, 8)}`,
	]);
});

test("any commit moves the baseline to HEAD; a requested one forces it over a pin in Helix", async (t) => {
	const w = workspace(t);
	const socket = join(w.root, "helix.sock");
	withEnv(t, { HELIX_COMMAND_SOCKET: socket });
	const { lines } = await fakeHelix(t, socket);
	const repo = realpathSync(w.repo);
	w.stage("a.rs", "a\n");
	w.stage("b.rs", "b\n");
	const head = w.commit();
	const outbox = join(repo, ".pi", "helix-review", "outbox");
	const state = () => JSON.parse(readFileSync(join(repo, ".pi", "helix-review", "state", "session-1.json"), "utf8"));

	const session = piSession(repo);
	t.after(() => session.emit("session_shutdown"));
	await session.emit("session_start", { reason: "startup" });
	await session.emit("agent_start");
	assert.equal(state().baseline.commit_oid, head);

	// The reviewer stages a.rs and asks for a commit; b.rs stays changed and unstaged.
	const batch = join(outbox, "9-1.json");
	writeFileSync(`${batch}.tmp`, JSON.stringify({ format: "helix-review-batch", version: 2, files: [], comments: [], request: { kind: "commit-staged" } }));
	renameSync(`${batch}.tmp`, batch);
	await until(() => !existsSync(batch), "the batch to be consumed");
	assert.equal(session.messages.length, 1);

	// The turn it starts commits; its end moves the baseline to the commit and says so.
	await session.emit("agent_start");
	w.stage("a.rs", "A\n");
	const committed = w.commit();
	writeFileSync(join(repo, "b.rs"), "B\n");
	lines.length = 0;
	await session.emit("agent_end", { messages: [] });
	assert.deepEqual(lines, [
		`review-baseline ${committed} force`,
		`review-baseline ${committed}`,
		`echo pi-1: Agent turn done, committed ${committed.slice(0, 8)}, 1 file still changed`,
	]);
	assert.deepEqual(state().baseline, { commit_oid: committed });
	assert.deepEqual(Object.keys(state().snapshots), [join(repo, "b.rs")]);

	// An ordinary turn that commits moves it too, without forcing.
	await session.emit("agent_start");
	w.stage("b.rs", "B\n");
	const ordinary = w.commit();
	lines.length = 0;
	await session.emit("agent_end", { messages: [] });
	assert.deepEqual(state().baseline, { commit_oid: ordinary });
	assert.deepEqual(lines, [
		`review-baseline ${ordinary}`,
		`echo pi-1: Agent turn done, no file changes since ${ordinary.slice(0, 8)}`,
	]);

	// A commit made outside pi, between turns, reaches Helix on the next poller tick.
	writeFileSync(join(repo, "c.rs"), "c\n");
	git(repo, "add", "c.rs");
	git(repo, "-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-q", "-m", "outside");
	const outside = git(repo, "rev-parse", "HEAD");
	lines.length = 0;
	await until(() => lines.includes(`review-baseline ${outside}`), "the poller to follow HEAD");
	assert.deepEqual(lines, [`review-baseline ${outside}`]);
	assert.deepEqual(state().baseline, { commit_oid: outside });
});

test("an ask handoff instructs the agent with every selection and nothing is sent back but the usual echoes", async (t) => {
	const w = workspace(t);
	const socket = join(w.root, "helix.sock");
	withEnv(t, { HELIX_COMMAND_SOCKET: socket });
	const { lines } = await fakeHelix(t, socket);
	const repo = realpathSync(w.repo);
	const file = join(repo, "a.rs");
	const blob = w.stage("a.rs", "fn a() {\n    let x = 1;\n}\n");
	const head = w.commit();
	const outbox = join(repo, ".pi", "helix-review", "outbox");

	const session = piSession(repo);
	t.after(() => session.emit("session_shutdown"));
	await session.emit("session_start", { reason: "startup" });
	await session.emit("agent_start");

	const comment = {
		path: file,
		side: "current",
		baseline: commitBase(head, blob),
		start_line: 2,
		end_line: 2,
		current_line: 2,
		selected_text: "    let x = 1;\n",
		context_before: "fn a() {\n",
		context_after: "}\n",
		enclosing_text: "fn a() {\n    let x = 1;\n}",
		comment: "rename x to count",
	};
	const batch = join(outbox, "7-1.json");
	writeFileSync(
		`${batch}.tmp`,
		JSON.stringify({ format: "helix-review-batch", version: 2, files: [], comments: [comment], request: { kind: "ask" } }),
	);
	renameSync(`${batch}.tmp`, batch);
	await until(() => !existsSync(batch), "the batch to be consumed");
	assert.equal(session.messages.length, 1);
	const content = session.messages[0].message.content;
	assert.match(content, /User request:\nThe user selected code in Helix and asked about it\. Do what the message asks\.\nSelection 1:/);
	assert.match(content, new RegExp(`Selection 1: ${file}, line 2\nSelected text:\n\`\`\`\n    let x = 1;\n\`\`\`\nEnclosing code:\n\`\`\`\nfn a\\(\\) \\{\n    let x = 1;\n\\}\n\`\`\`\nMessage:\n\`\`\`\nrename x to count\n\`\`\`$`));

	// The turn it starts edits; its end refreshes and gives the ordinary summary, no reply.
	await session.emit("agent_start");
	writeFileSync(file, "fn a() {\n    let count = 1;\n}\n");
	lines.length = 0;
	await session.emit("agent_end", { messages: [{ role: "assistant", content: [{ type: "text", text: "Renamed." }] }] });
	assert.ok(!lines.some((line) => line.startsWith("review-reply")), lines.join("\n"));
	assert.ok(!existsSync(join(repo, ".pi", "helix-review", "replies")));
	assert.deepEqual(lines, [
		`review-baseline ${head}`,
		`echo pi-1: Agent turn done, 1 file changed since ${head.slice(0, 8)} (:review-files)`,
	]);
});

test("a Helix started after pi, or restarted, is attached once its socket appears", async (t) => {
	const w = workspace(t);
	const socket = join(w.root, "helix.sock");
	withEnv(t, { HELIX_COMMAND_SOCKET: socket });
	const repo = realpathSync(w.repo);
	w.stage("a.rs", "a\n");
	const head = w.commit();
	const outbox = join(repo, ".pi", "helix-review", "outbox");

	const session = piSession(repo);
	t.after(() => session.emit("session_shutdown"));
	await session.emit("session_start", { reason: "startup" });
	await new Promise((wake) => setTimeout(wake, 100));
	const first = await fakeHelix(t, socket);
	await until(() => first.lines.length === 2, "the late Helix to be attached");
	assert.deepEqual(first.lines, [`review-handoff ${outbox}`, "echo pi hook attached as pi-1"]);

	await session.emit("agent_start");
	await first.quit();
	assert.ok(!existsSync(socket));
	await session.emit("agent_end", { messages: [] });
	const second = await fakeHelix(t, socket);
	await until(() => second.lines.length === 3, "the restarted Helix to be attached");
	assert.deepEqual(second.lines, [`review-handoff ${outbox}`, `review-baseline ${head}`, `echo pi hook attached as pi-1, baseline ${head.slice(0, 8)}`]);
});

test("a batch about files outside the repository is dropped without a turn or a receipt", async (t) => {
	const w = workspace(t);
	const socket = join(w.root, "helix.sock");
	withEnv(t, { HELIX_COMMAND_SOCKET: socket });
	const { lines } = await fakeHelix(t, socket);
	const warn = t.mock.method(console, "warn", () => {});
	const repo = realpathSync(w.repo);
	w.stage("a.rs", "a\n");
	const head = w.commit();
	const outside = join(w.root, "file.txt");
	writeFileSync(outside, "x\n");
	const outbox = join(repo, ".pi", "helix-review", "outbox");

	const session = piSession(repo);
	t.after(() => session.emit("session_shutdown"));
	await session.emit("session_start", { reason: "startup" });
	await session.emit("agent_start");
	lines.splice(0);
	const batch = join(outbox, "3-1.json");
	writeFileSync(
		batch,
		JSON.stringify({
			format: "helix-review-batch",
			version: 2,
			files: [{ path: outside, baseline: commitBase(head, null) }],
			comments: [],
			request: { kind: "message", text: "not for this agent" },
		}),
	);
	await until(() => !existsSync(batch), "the batch to be dropped");
	await new Promise((wake) => setTimeout(wake, 100));
	assert.equal(session.messages.length, 0);
	assert.deepEqual(lines, [], "no receipt is echoed");
	assert.equal(warn.mock.callCount(), 1);
	const state = JSON.parse(readFileSync(join(repo, ".pi", "helix-review", "state", "session-1.json"), "utf8"));
	assert.deepEqual(state.baseline, { commit_oid: head });
});

test("several sessions in one repository each take only the batches meant for them", async (t) => {
	const w = workspace(t);
	withEnv(t, { HELIX_COMMAND_SOCKET: join(w.root, "missing.sock") });
	const repo = realpathSync(w.repo);
	const [a, b] = [join(repo, "a.rs"), join(repo, "b.rs")];
	w.stage("a.rs", "a\n");
	w.stage("b.rs", "b\n");
	const head = w.commit();
	const dir = join(repo, ".pi", "helix-review");
	const outbox = join(dir, "outbox");
	// An earlier version's single state.json moves into the first session's file.
	mkdirSync(dir, { recursive: true });
	writeFileSync(join(dir, "state.json"), JSON.stringify({ snapshots: {}, baseline: { commit_oid: head, acknowledged: false } }));

	const one = piSession(repo, "session-1");
	t.after(() => one.emit("session_shutdown"));
	await one.emit("session_start", { reason: "startup" });
	const state = (id: string) => JSON.parse(readFileSync(join(dir, "state", `${id}.json`), "utf8"));
	assert.ok(!existsSync(join(dir, "state.json")));
	assert.equal(state("session-1").baseline.commit_oid, head);
	assert.equal(state("session-1").pid, process.pid);

	const two = piSession(repo, "session-2");
	t.after(() => two.emit("session_shutdown"));
	await two.emit("session_start", { reason: "startup" });
	// Session 2 runs a turn that edits nothing; session 1 edits a.rs in a later turn.
	await two.emit("agent_start");
	await two.emit("agent_end", { messages: [] });
	await new Promise((wake) => setTimeout(wake, 5));
	await one.emit("agent_start");
	writeFileSync(a, "a edited\n");
	await one.emit("agent_end", { messages: [] });
	assert.ok(state("session-1").edited[a] > 0);
	assert.equal(state("session-2").edited?.[a], undefined);

	const send = (name: string, file: string, extra: object = {}) => {
		const path = join(outbox, name);
		const doc = { format: "helix-review-batch", version: 2, files: [], comments: [{ ...comment(file), comment: name }], ...extra };
		writeFileSync(`${path}.tmp`, JSON.stringify(doc));
		renameSync(`${path}.tmp`, path);
		return path;
	};
	const comment = (path: string) => ({
		path,
		side: "current",
		baseline: commitBase(head, null),
		start_line: 1,
		end_line: 1,
		current_line: 1,
		selected_text: "x",
		context_before: "",
		context_after: "",
	});
	const got = (session: typeof one) => session.messages.map((m) => m.message.details.files[0].review_comments[0].comment);

	// One at a time: a batch routed by ownership is rewritten for its owner before it is handled.
	const handled = () => got(one).length + got(two).length;
	for (const [i, path] of [() => send("1-1.json", a), () => send("1-2.json", b), () => send("1-3.json", b, { target: "session-1" })].entries()) {
		path();
		await until(() => handled() === i + 1, `handoff ${i + 1}`);
	}
	assert.deepEqual(got(one), ["1-1.json", "1-3.json"], "the editor of a.rs, then the target");
	assert.deepEqual(got(two), ["1-2.json"], "the longest idle session");
	assert.deepEqual(readdirSync(outbox), []);

	// A claim left over 10 minutes ago by a session that is gone goes back to the outbox on the
	// next session start; a fresh one stays claimed.
	const doc = (name: string) => JSON.stringify({ format: "helix-review-batch", version: 2, files: [], comments: [{ ...comment(b), comment: name }], target: "session-3" });
	const stale = join(outbox, "1-4.session-9.claimed");
	const fresh = join(outbox, "1-5.session-9.claimed");
	writeFileSync(stale, doc("1-4.json"));
	writeFileSync(fresh, doc("1-5.json"));
	const old = new Date(Date.now() - 11 * 60 * 1000);
	utimesSync(stale, old, old);
	const three = piSession(repo, "session-3");
	t.after(() => three.emit("session_shutdown"));
	await three.emit("session_start", { reason: "startup" });
	await until(() => got(three).length === 1, "the swept batch");
	assert.deepEqual(got(three), ["1-4.json"]);
	assert.deepEqual(readdirSync(outbox), ["1-5.session-9.claimed"]);
});

test("a batch nobody's changes explain goes to the least busy session, and dead sessions' state is removed", async (t) => {
	const w = workspace(t);
	withEnv(t, { HELIX_COMMAND_SOCKET: join(w.root, "missing.sock") });
	const repo = realpathSync(w.repo);
	const file = join(repo, "a.rs");
	w.stage("a.rs", "a\n");
	const head = w.commit();
	const dir = join(repo, ".pi", "helix-review");
	const outbox = join(dir, "outbox");
	mkdirSync(join(dir, "state"), { recursive: true });
	const dead = join(dir, "state", "session-dead.json");
	writeFileSync(dead, JSON.stringify({ snapshots: {}, label: "pi-1", pid: 2 ** 22 + 12345, baseline: { commit_oid: head } }));

	const tick = () => new Promise((wake) => setTimeout(wake, 5));
	const sessions: ReturnType<typeof piSession>[] = [];
	for (const id of ["idle-long", "idle-short", "busy-early", "busy-late"]) {
		const s = piSession(repo, id);
		t.after(() => s.emit("session_shutdown"));
		await s.emit("session_start", { reason: "startup" });
		sessions.push(s);
		await tick();
	}
	const [idleLong, idleShort, busyEarly, busyLate] = sessions;
	assert.ok(!existsSync(dead), "a dead session's state file is removed at open");
	await busyEarly.emit("agent_start");
	await tick();
	await busyLate.emit("agent_start");

	let n = 0;
	const send = () => {
		const path = join(outbox, `9-${++n}.json`);
		const comment = { path: file, side: "current", baseline: commitBase(head, null), start_line: 1, end_line: 1, current_line: 1, selected_text: "a", context_before: "", context_after: "", comment: "c" };
		writeFileSync(`${path}.tmp`, JSON.stringify({ format: "helix-review-batch", version: 2, files: [], comments: [comment] }));
		renameSync(`${path}.tmp`, path);
	};
	const counts = () => sessions.map((s) => s.messages.length);
	send();
	await until(() => counts().reduce((a, b) => a + b) === 1, "the first handoff");
	assert.deepEqual(counts(), [1, 0, 0, 0], "the longest idle of the sessions not in a turn");

	await tick();
	await idleShort.emit("agent_start");
	await idleLong.emit("agent_start");
	send();
	await until(() => counts().reduce((a, b) => a + b) === 2, "the second handoff");
	assert.deepEqual(counts(), [1, 0, 1, 0], "with every session in a turn, the one whose turn started first");
	assert.deepEqual(readdirSync(outbox), []);
});

test("sessions are named pi-N in Helix, and every session's review follows HEAD", async (t) => {
	const w = workspace(t);
	const socket = join(w.root, "helix.sock");
	withEnv(t, { HELIX_COMMAND_SOCKET: socket });
	const { lines } = await fakeHelix(t, socket);
	const repo = realpathSync(w.repo);
	const file = join(repo, "a.rs");
	w.stage("a.rs", "a\n");
	const first = w.commit();
	const outbox = join(repo, ".pi", "helix-review", "outbox");

	const one = piSession(repo, "session-1");
	t.after(() => one.emit("session_shutdown"));
	await one.emit("session_start", { reason: "startup" });
	const two = piSession(repo, "session-2");
	t.after(() => two.emit("session_shutdown"));
	await two.emit("session_start", { reason: "startup" });
	assert.deepEqual(lines.splice(0), [
		`review-handoff ${outbox}`,
		"echo pi hook attached as pi-1",
		`review-handoff ${outbox}`,
		"echo pi hook attached as pi-2",
	]);

	// pi-1's turn starts on the first commit; after a later commit, pi-2's turn reviews against it.
	await one.emit("agent_start");
	await new Promise((wake) => setTimeout(wake, 1100));
	w.stage("a.rs", "b\n");
	const second = w.commit();
	await two.emit("agent_start");
	assert.deepEqual(lines.splice(0), [
		`review-handoff ${outbox}`,
		`review-baseline ${first}`,
		"echo pi-1: Agent turn started",
		`review-handoff ${outbox}`,
		`review-baseline ${second}`,
		"echo pi-2: Agent turn started",
	]);

	const batch = join(outbox, "5-1.json");
	const doc = { format: "helix-review-batch", version: 2, files: [{ path: file, baseline: commitBase(second, null) }], comments: [], target: "pi-1" };
	writeFileSync(`${batch}.tmp`, JSON.stringify(doc));
	renameSync(`${batch}.tmp`, batch);
	await until(() => lines.some((line) => line.includes("received your review")), "the receipt");
	assert.equal(two.messages.length, 0);
	assert.equal(one.messages.length, 1);
	assert.deepEqual(lines.splice(0), ["echo pi-1 received your review: 1 file with edits"]);
	await two.emit("agent_end", { messages: [] });
	assert.equal(lines[0], `review-baseline ${second}`);
});

test("a commit the reviewer asked for moves every session's review to it, past stale baselines", async (t) => {
	const w = workspace(t);
	const socket = join(w.root, "helix.sock");
	withEnv(t, { HELIX_COMMAND_SOCKET: socket, HELIX_GH: "false" });
	const { lines } = await fakeHelix(t, socket);
	const repo = realpathSync(w.repo);
	w.stage("a.rs", "a\n");
	const first = w.commit();
	// No base branch, so a stale side baseline is not remapped onto the merge base.
	git(repo, "branch", "-m", "work");
	const outbox = join(repo, ".pi", "helix-review", "outbox");
	const sent = () => lines.filter((line) => line.startsWith("review-baseline ")).at(-1);

	const one = piSession(repo, "session-1");
	t.after(() => one.emit("session_shutdown"));
	await one.emit("session_start", { reason: "startup" });
	const two = piSession(repo, "session-2");
	t.after(() => two.emit("session_shutdown"));
	await two.emit("session_start", { reason: "startup" });
	// pi-2's turn starts on the first commit; pi-1 starts after a second one.
	await two.emit("agent_start");
	await new Promise((wake) => setTimeout(wake, 1100));
	w.stage("b.rs", "b\n");
	const second = w.commit();
	await one.emit("agent_start");
	assert.equal(sent(), `review-baseline ${second}`);
	// A third live session holds a stale baseline on a side branch, older than anything to come.
	const side = execFileSync("git", ["-c", "user.name=t", "-c", "user.email=t@example.com", "commit-tree", `${first}^{tree}`, "-p", first, "-m", "side"], { cwd: repo, env: gitEnv, encoding: "utf8" }).trim();
	writeFileSync(join(repo, ".pi", "helix-review", "state", "session-3.json"), JSON.stringify({ snapshots: {}, baseline: { commit_oid: side, acknowledged: false }, pid: process.pid }));

	// The reviewer asks pi-1 to commit; its turn commits, and the review moves on for everyone.
	const batch = join(outbox, "9-1.json");
	writeFileSync(`${batch}.tmp`, JSON.stringify({ format: "helix-review-batch", version: 2, files: [], comments: [], request: { kind: "commit-staged" }, target: "pi-1" }));
	renameSync(`${batch}.tmp`, batch);
	await until(() => !existsSync(batch), "the batch to be consumed");
	await one.emit("agent_start");
	await new Promise((wake) => setTimeout(wake, 1100));
	w.stage("a.rs", "A\n");
	const committed = w.commit();
	lines.length = 0;
	await one.emit("agent_end", { messages: [] });
	assert.equal(sent(), `review-baseline ${committed}`);
	assert.deepEqual(lines.filter((line) => line.endsWith(" force")), [`review-baseline ${committed} force`], "the commit releases a pin in Helix");
	lines.length = 0;
	await two.emit("agent_end", { messages: [] });
	assert.equal(sent(), `review-baseline ${committed}`, "pi-2's older and pi-3's side baselines are lifted");

	// An ordinary commit afterwards moves the review too, without forcing.
	await one.emit("agent_start");
	w.stage("c.rs", "c\n");
	const ordinary = w.commit();
	lines.length = 0;
	await one.emit("agent_end", { messages: [] });
	await two.emit("agent_end", { messages: [] });
	assert.equal(sent(), `review-baseline ${ordinary}`);
	assert.ok(lines.every((line) => !line.endsWith(" force")), "an ordinary turn never forces");
});

test("pi's footer shows the session's Helix label, its worktree hooks and a missing Helix", async (t) => {
	const w = workspace(t);
	const socket = join(w.root, "helix.sock");
	withEnv(t, { HELIX_COMMAND_SOCKET: socket });
	const repo = realpathSync(w.repo);
	w.stage("a.rs", "a\n");
	w.commit();
	const dir = join(w.root, "wt");
	git(repo, "worktree", "add", "-q", "-b", "wt", dir);
	const file = join(realpathSync(dir), "b.rs");

	let helix = await fakeHelix(t, socket);
	const session = piSession(repo);
	await session.emit("session_start", { reason: "startup" });
	assert.equal(session.statuses.at(-1), "hx:pi-1");
	// The poller notices the socket go and come back.
	await helix.quit();
	await until(() => session.statuses.at(-1) === "hx:pi-1 (no helix)", "the missing Helix");
	helix = await fakeHelix(t, socket);
	await until(() => session.statuses.at(-1) === "hx:pi-1", "the new Helix");
	await session.emit("agent_start");
	writeFileSync(file, "b\n");
	await session.emit("tool_result", { toolName: "write", input: { path: file }, content: [], isError: false });
	assert.equal(session.statuses.at(-1), "hx:pi-1+1");

	// Outside a repository there is nothing to show.
	const elsewhere = piSession(w.root, "session-2");
	await elsewhere.emit("session_start", { reason: "startup" });
	assert.deepEqual(elsewhere.statuses, [undefined]);
});

test("edits in a worktree of the repository get their own hook, Helix and mailbox", async (t) => {
	const w = workspace(t);
	const runtime = join(w.root, "run");
	mkdirSync(join(runtime, "helix"), { recursive: true });
	withEnv(t, { HELIX_COMMAND_SOCKET: undefined, XDG_RUNTIME_DIR: runtime });
	const helixAt = (dir: string) => {
		const id = createHash("sha256").update(dir).digest("hex").slice(0, 32);
		return fakeHelix(t, join(runtime, "helix", `${id}.sock`));
	};
	const repo = realpathSync(w.repo);
	w.stage("a.rs", "a\n");
	w.commit();
	const worktree = (name: string) => {
		const dir = join(w.root, name);
		git(repo, "worktree", "add", "-q", "-b", name, dir);
		const top = realpathSync(dir);
		writeFileSync(join(top, `${name}.rs`), "x\n");
		git(top, "add", "--", `${name}.rs`);
		git(top, "-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-q", "-m", name);
		return { top, head: git(top, "rev-parse", "HEAD"), file: join(top, `${name}.rs`), outbox: join(top, ".pi", "helix-review", "outbox") };
	};
	const wt = worktree("wt");
	const main = await helixAt(repo);
	const other = await helixAt(wt.top);

	const session = piSession(repo);
	t.after(() => session.emit("session_shutdown"));
	await session.emit("session_start", { reason: "startup" });
	await session.emit("agent_start");
	main.lines.splice(0);
	assert.deepEqual(other.lines, []);

	// An edit in the worktree opens its hook, pinned to the worktree's HEAD.
	writeFileSync(wt.file, "y\n");
	await session.emit("tool_result", { toolName: "edit", input: { path: wt.file }, content: [], isError: false });
	assert.ok(existsSync(wt.outbox));
	assert.deepEqual(other.lines.slice(0, 3), [`review-handoff ${wt.outbox}`, `review-baseline ${wt.head}`, "echo pi-1: Agent turn started"]);
	await session.emit("agent_end", { messages: [] });
	const state = JSON.parse(readFileSync(join(wt.top, ".pi", "helix-review", "state", "session-1.json"), "utf8"));
	assert.deepEqual(Object.keys(state.snapshots), [wt.file]);
	assert.equal(other.lines.at(-1), `echo pi-1: Agent turn done, 1 file changed since ${wt.head.slice(0, 8)} (:review-files)`);
	assert.ok(main.lines.every((line) => !line.includes(wt.top)), "the main checkout's Helix sees nothing of the worktree");
	assert.equal(main.lines.at(-1), `echo pi-1: Agent turn done, no file changes since ${git(repo, "rev-parse", "HEAD").slice(0, 8)}`);

	// A review saved in the worktree's Helix reaches the session.
	writeFileSync(wt.file, "Y\n");
	const batch = join(wt.outbox, "7-1.json");
	const blob = git(wt.top, "rev-parse", `HEAD:wt.rs`);
	writeFileSync(`${batch}.tmp`, JSON.stringify({ format: "helix-review-batch", version: 2, files: [{ path: wt.file, baseline: commitBase(wt.head, blob) }], comments: [] }));
	renameSync(`${batch}.tmp`, batch);
	await until(() => session.messages.length === 1 && !existsSync(batch), "the worktree's batch");
	await until(() => other.lines.at(-1) === "echo pi-1 received your review: 1 file with edits", "the receipt in the worktree's Helix");

	// Bash changing another worktree is noticed at the end of the turn.
	const wt2 = worktree("wt2");
	const third = await helixAt(wt2.top);
	await session.emit("agent_start");
	writeFileSync(wt2.file, "z\n");
	await session.emit("tool_result", { toolName: "bash", input: { command: "sed" }, content: [], isError: false });
	await session.emit("agent_end", { messages: [] });
	assert.deepEqual(third.lines.slice(0, 2), [`review-handoff ${wt2.outbox}`, `review-baseline ${wt2.head}`]);
	assert.equal(third.lines.at(-1), `echo pi-1: Agent turn done, 1 file changed since ${wt2.head.slice(0, 8)} (:review-files)`);
});

test("the ownership ledger tracks who changed which lines until they are committed", async (t) => {
	const w = workspace(t);
	withEnv(t, { HELIX_COMMAND_SOCKET: join(w.root, "missing.sock") });
	const repo = realpathSync(w.repo);
	const file = join(repo, "a.rs");
	const lines = (edit: (lines: string[]) => void) => {
		const all = readFileSync(file, "utf8").split("\n").slice(0, -1);
		edit(all);
		writeFileSync(file, `${all.join("\n")}\n`);
	};
	w.stage("a.rs", Array.from({ length: 10 }, (_, i) => `${i + 1}\n`).join(""));
	w.commit();
	const ledger = join(repo, ".pi", "helix-review", "state", "ownership.json");
	const owners = () => {
		if (!existsSync(ledger)) return [];
		const entry = JSON.parse(readFileSync(ledger, "utf8")).files[file];
		return (entry?.ranges ?? []).map(({ start, end, owner, agent }: { start: number; end: number; owner: string; agent?: string }) => [start, end, owner, agent]);
	};
	const edited = { toolName: "edit", input: { path: file }, content: [], isError: false };

	const one = piSession(repo, "session-1");
	t.after(() => one.emit("session_shutdown"));
	await one.emit("session_start", { reason: "startup" });
	const two = piSession(repo, "session-2");
	t.after(() => two.emit("session_shutdown"));
	await two.emit("session_start", { reason: "startup" });

	// pi-1 changes lines 2-3.
	await one.emit("agent_start");
	lines((l) => l.splice(1, 2, "two", "three"));
	await one.emit("tool_result", edited);
	assert.deepEqual(owners(), [[2, 4, "pi-1", "pi-1"]]);
	await one.emit("agent_end", { messages: [] });

	// pi-2 overwrites line 3 (last writer wins) and changes line 8.
	await two.emit("agent_start");
	lines((l) => {
		l[2] = "THREE";
		l[7] = "eight";
	});
	await two.emit("tool_result", edited);
	assert.deepEqual(owners(), [
		[2, 3, "pi-1", "pi-1"],
		[3, 4, "pi-2", "pi-2"],
		[8, 9, "pi-2", "pi-2"],
	]);

	// Two lines inserted at the top by bash shift every range.
	lines((l) => l.unshift("a", "b"));
	await two.emit("tool_result", { toolName: "bash", input: { command: "sed" }, content: [], isError: false });
	assert.deepEqual(owners(), [
		[1, 3, "pi-2", "pi-2"],
		[4, 5, "pi-1", "pi-1"],
		[5, 6, "pi-2", "pi-2"],
		[10, 11, "pi-2", "pi-2"],
	]);
	await two.emit("agent_end", { messages: [] });

	// With no turn running, an edit is the user's, keeping the agent it overwrote.
	lines((l) => {
		l[4] = "three!";
	});
	await until(() => owners().some(([, , owner]: string[]) => owner === "user"), "the poller to attribute the user's edit");
	assert.deepEqual(owners(), [
		[1, 3, "pi-2", "pi-2"],
		[4, 5, "pi-1", "pi-1"],
		[5, 6, "user", "pi-2"],
		[10, 11, "pi-2", "pi-2"],
	]);

	// A commit of everything but the two inserted lines leaves only those.
	const partial = readFileSync(file, "utf8").split("\n").slice(2).join("\n");
	const blob = execFileSync("git", ["hash-object", "-w", "--stdin"], { cwd: repo, env: gitEnv, input: partial, encoding: "utf8" }).trim();
	git(repo, "update-index", "--cacheinfo", `100644,${blob},a.rs`);
	w.commit();
	await until(() => owners().length === 1, "the poller to prune the committed ranges");
	assert.deepEqual(owners(), [[1, 3, "pi-2", "pi-2"]]);
});

test("only readable text files of the repository are attributed, and a failing one is skipped with one warning", async (t) => {
	const w = workspace(t);
	withEnv(t, { HELIX_COMMAND_SOCKET: join(w.root, "missing.sock") });
	const repo = realpathSync(w.repo);
	const [a, b] = [join(repo, "a.rs"), join(repo, "b.rs")];
	w.stage("a.rs", "a\n");
	w.stage("b.rs", "b\n");
	w.commit();
	const dir = join(repo, ".pi", "helix-review", "state");
	const ledger = join(dir, "ownership.json");
	const files = () => JSON.parse(readFileSync(ledger, "utf8")).files as Record<string, { ranges: { owner: string }[] }>;
	const outside = join(w.root, "outside.txt");
	writeFileSync(outside, "x\n");
	// A ledger of an earlier version, with a file outside the repository and one since deleted.
	mkdirSync(dir, { recursive: true });
	const range = [{ start: 1, end: 2, owner: "pi-1", at: 1 }];
	writeFileSync(ledger, JSON.stringify({ files: { [outside]: { blob: "0", ranges: range }, [join(repo, "gone.rs")]: { blob: "0", ranges: range } } }));
	const warnings: string[] = [];
	const warn = console.warn;
	console.warn = (...args: unknown[]) => void warnings.push(args.join(" "));
	t.after(() => {
		console.warn = warn;
	});

	const session = piSession(repo);
	t.after(() => session.emit("session_shutdown"));
	await session.emit("session_start", { reason: "startup" });
	await session.emit("agent_start");
	assert.deepEqual(files(), {}, "the stale entries are dropped");

	// Files outside the repository, and inside it too large, are not attributed.
	const big = join(repo, "big.json");
	writeFileSync(big, `${"x".repeat(2 * 1024 * 1024)}\n`);
	for (const path of [outside, big]) {
		writeFileSync(path, readFileSync(path, "utf8") + "y\n");
		await session.emit("tool_result", { toolName: "edit", input: { path }, content: [], isError: false });
	}
	assert.deepEqual(Object.keys(files()), []);

	// b.rs becomes unreadable: a.rs is still attributed, and b.rs is warned about once.
	writeFileSync(b, "b edited\n");
	await session.emit("tool_result", { toolName: "edit", input: { path: b }, content: [], isError: false });
	chmodSync(b, 0o000);
	writeFileSync(a, "a edited\n");
	const bash = { toolName: "bash", input: { command: "sed" }, content: [], isError: false };
	await session.emit("tool_result", bash);
	await session.emit("tool_result", bash);
	assert.deepEqual(Object.keys(files()).sort(), [a, b]);
	assert.equal(files()[a].ranges[0].owner, "pi-1");
	assert.equal(warnings.length, 1, warnings.join("\n"));
	assert.ok(warnings[0].startsWith(`helix-review: ownership of ${b} not updated:`), warnings[0]);
	await session.emit("agent_end", { messages: [] });
});

test("comments go to the owners of their hunks, and each owner commits only its staged hunks", async (t) => {
	const w = workspace(t);
	withEnv(t, { HELIX_COMMAND_SOCKET: join(w.root, "missing.sock") });
	const repo = realpathSync(w.repo);
	const file = join(repo, "a.rs");
	const edit = (line: number, text: string) => {
		const all = readFileSync(file, "utf8").split("\n");
		all[line - 1] = text;
		writeFileSync(file, all.join("\n"));
	};
	w.stage("a.rs", Array.from({ length: 10 }, (_, i) => `${i + 1}\n`).join(""));
	const head = w.commit();
	const blob = git(repo, "rev-parse", "HEAD:a.rs");
	const outbox = join(repo, ".pi", "helix-review", "outbox");
	const edited = { toolName: "edit", input: { path: file }, content: [], isError: false };

	const one = piSession(repo, "session-1");
	t.after(() => one.emit("session_shutdown"));
	await one.emit("session_start", { reason: "startup" });
	const two = piSession(repo, "session-2");
	t.after(() => two.emit("session_shutdown"));
	await two.emit("session_start", { reason: "startup" });
	// pi-1 changes line 2, pi-2 line 8.
	await one.emit("agent_start");
	edit(2, "two");
	await one.emit("tool_result", edited);
	await one.emit("agent_end", { messages: [] });
	await two.emit("agent_start");
	edit(8, "eight");
	await two.emit("tool_result", edited);
	await two.emit("agent_end", { messages: [] });

	const send = (name: string, doc: object) => {
		const batch = join(outbox, name);
		writeFileSync(`${batch}.tmp`, JSON.stringify({ format: "helix-review-batch", version: 2, files: [], comments: [], ...doc }));
		renameSync(`${batch}.tmp`, batch);
	};
	const comment = (line: number, text: string) => ({
		path: file,
		side: "current",
		baseline: commitBase(head, blob),
		start_line: line,
		end_line: line,
		current_line: line,
		selected_text: "x",
		context_before: "",
		context_after: "",
		comment: text,
	});
	const commented = (session: ReturnType<typeof piSession>) =>
		session.messages.flatMap((m) => m.message.details.files.flatMap((f: { review_comments: { comment: string }[] }) => f.review_comments.map((c) => c.comment)));

	// One batch with a comment on each session's hunk is split between them.
	send("1-1.json", { comments: [comment(2, "on two"), comment(8, "on eight")] });
	await until(() => one.messages.length === 1 && two.messages.length === 1, "each owner's share");
	assert.deepEqual(commented(one), ["on two"]);
	assert.deepEqual(commented(two), ["on eight"]);
	assert.deepEqual(readdirSync(outbox), []);

	// With both hunks staged, pi-1 commits only its own, from a patch, through a temporary index.
	git(repo, "add", "a.rs");
	send("1-2.json", { request: { kind: "commit-staged" } });
	await until(() => one.messages.length === 2, "pi-1's commit request");
	assert.equal(two.messages.length, 1);
	const first = one.messages[1].message;
	assert.deepEqual(first.details.commit_patch.hunks, ["a.rs:2-2"]);
	const patch = first.details.commit_patch.patch;
	assert.equal(patch, join(repo, ".pi", "helix-review", "commits", "1-2-pi-1.patch"));
	assert.match(readFileSync(patch, "utf8"), /^diff --git a\/a.rs b\/a.rs\n--- a\/a.rs\n\+\+\+ b\/a.rs\n@@ [^\n]*\n( [^\n]*\n)*-2\n\+two\n( [^\n]*\n)*$/);
	assert.doesNotMatch(readFileSync(patch, "utf8"), /eight/);
	assert.match(first.content, /GIT_INDEX_FILE=\$tmp git apply --cached/);
	const tmp = join(w.root, "index.tmp");
	const env = { ...gitEnv, GIT_INDEX_FILE: tmp };
	const run = (...args: string[]) => execFileSync("git", args, { cwd: repo, env, encoding: "utf8" });
	run("read-tree", "HEAD");
	run("apply", "--cached", patch);
	run("-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-q", "-m", "pi-1's share");
	rmSync(tmp);
	assert.equal(git(repo, "show", "HEAD:a.rs").split("\n")[7], "8");

	// At the end of its turn, pi-2 is asked to commit the rest: the real index, as usual.
	await one.emit("agent_end", { messages: [] });
	await until(() => two.messages.length === 2, "pi-2's commit request");
	const rest = two.messages[1].message;
	assert.deepEqual(rest.details.request, { kind: "commit-staged" });
	assert.equal(rest.details.commit_patch, undefined);
	assert.match(rest.content, /Commit exactly what is staged in the git index/);
	assert.equal(git(repo, "diff", "--cached", "--stat"), "a.rs | 2 +-\n 1 file changed, 1 insertion(+), 1 deletion(-)");
});

test("a Helix restarted on the same socket is attached again without waiting for a failed send", async (t) => {
	const w = workspace(t);
	const socket = join(w.root, "helix.sock");
	withEnv(t, { HELIX_COMMAND_SOCKET: socket });
	const repo = realpathSync(w.repo);
	w.stage("a.rs", "a\n");
	w.commit();
	const outbox = join(repo, ".pi", "helix-review", "outbox");
	const first = await fakeHelix(t, socket);

	const session = piSession(repo);
	t.after(() => session.emit("session_shutdown"));
	await session.emit("session_start", { reason: "startup" });
	assert.deepEqual(first.lines, [`review-handoff ${outbox}`, "echo pi hook attached as pi-1"]);

	// Helix quits and a new one listens on the same path; pi sends nothing meanwhile.
	await first.quit();
	const second = await fakeHelix(t, socket);
	await until(() => second.lines.length === 2, "the new Helix to be attached");
	assert.deepEqual(second.lines, [`review-handoff ${outbox}`, "echo pi hook attached as pi-1"]);
	assert.equal(first.lines.length, 2);
});

test("baselines follow the branch through a rebase onto a newer base", async (t) => {
	const log = t.mock.method(console, "info", () => {});
	// main: c0, then m1 after the topic branched; topic: c0, c1, c2.
	const scenario = async (label: string, rebase: string[]) => {
		const w = workspace(t);
		const socket = join(w.root, `${label}.sock`);
		withEnv(t, { HELIX_COMMAND_SOCKET: socket, HELIX_GH: "false" });
		const { lines } = await fakeHelix(t, socket);
		const repo = realpathSync(w.repo);
		const commit = (name: string, text: string) => {
			w.stage(name, text);
			return w.commit();
		};
		git(repo, "checkout", "-q", "-B", "main");
		const c0 = commit("a.txt", "a\n");
		git(repo, "checkout", "-q", "-b", "topic");
		const c1 = commit("t1.txt", "1\n");
		const c2 = commit("t2.txt", "2\n");
		git(repo, "checkout", "-q", "main");
		const m1 = commit("m.txt", "m\n");
		git(repo, "checkout", "-q", "topic");
		git(repo, "-c", "user.name=t", "-c", "user.email=t@example.com", "rebase", "-q", ...rebase);
		const head = git(repo, "rev-parse", "HEAD");
		const stateDir = join(repo, ".pi", "helix-review", "state");
		mkdirSync(stateDir, { recursive: true });
		const pin = (id: string, oid: string, pid?: number, acknowledged = false) =>
			writeFileSync(join(stateDir, `${id}.json`), JSON.stringify({ snapshots: {}, baseline: { commit_oid: oid, acknowledged }, pid }));
		const sent = () => lines.find((line) => line.startsWith("review-baseline "))?.slice("review-baseline ".length);
		// Starts a session and returns the baseline its first turn sends to Helix.
		const turn = async (id: string) => {
			const session = piSession(repo, id);
			t.after(() => session.emit("session_shutdown"));
			await session.emit("session_start", { reason: "startup" });
			lines.splice(0);
			await session.emit("agent_start");
			return sent();
		};
		// Starts a session and returns the baseline it restores in Helix on attaching.
		const attach = async (id: string) => {
			const session = piSession(repo, id);
			t.after(() => session.emit("session_shutdown"));
			lines.splice(0);
			await session.emit("session_start", { reason: "startup" });
			return sent();
		};
		const state = (id: string) => JSON.parse(readFileSync(join(stateDir, `${id}.json`), "utf8"));
		return { c0, c1, c2, m1, head, pin, turn, attach, state, stateDir };
	};

	// (a)-(c) Whatever the rebase did to a session's baseline (rewrote the tip, moved the base,
	// dropped the commit), its next turn reviews against the rebased HEAD, persisted.
	for (const [label, rebase, stale] of [
		["a", ["main"], "c2"],
		["b", ["main"], "c0"],
		["c", ["--onto", "main", "HEAD~1"], "c1"],
	] as const) {
		const s = await scenario(label, [...rebase]);
		s.pin("session-1", s[stale]);
		assert.equal(await s.turn("session-1"), s.head, label);
		assert.deepEqual(s.state("session-1").baseline, { commit_oid: s.head }, label);
	}

	// (d) Attaching follows HEAD too, and leaves another session's file alone.
	const d = await scenario("d", ["main"]);
	d.pin("session-1", d.c2, process.pid);
	d.pin("session-2", d.c0, undefined, true);
	assert.equal(await d.attach("session-2"), d.head);
	assert.equal(d.state("session-1").baseline.commit_oid, d.c2);

	// (e) A commit floor the rebase rewrote is remapped onto the rewritten commit.
	const e = await scenario("e", ["main"]);
	writeFileSync(join(e.stateDir, "session-1.json"), JSON.stringify({ snapshots: {}, baseline: { commit_oid: e.c0 }, pid: process.pid, commit_floor: e.c2 }));
	assert.equal(await e.attach("session-2"), e.head, "the rewritten floor");
	assert.ok(log.mock.calls.some((call) => call.arguments[0] === `helix-review: baseline ${e.c2.slice(0, 8)} rewritten as ${e.head.slice(0, 8)} by a rebase`));
});

test("an unreachable Helix never fails the hook, and an empty handoff is dropped", async (t) => {
	const w = workspace(t);
	withEnv(t, { HELIX_COMMAND_SOCKET: join(w.root, "missing.sock") });
	const repo = realpathSync(w.repo);
	const file = join(repo, "a.rs");
	w.stage("a.rs", "a\n");
	const head = w.commit();
	const outbox = join(repo, ".pi", "helix-review", "outbox");

	const session = piSession(repo);
	t.after(() => session.emit("session_shutdown"));
	await session.emit("session_start", { reason: "startup" });
	await session.emit("agent_start");
	await session.emit("tool_result", { toolName: "edit", input: {}, content: [], isError: false });
	await new Promise((wake) => setTimeout(wake, 300));
	await session.emit("agent_end", { messages: [] });

	const batch = join(outbox, "2-2.json");
	writeFileSync(
		batch,
		JSON.stringify({
			format: "helix-review-batch",
			version: 2,
			files: [{ path: file, baseline: commitBase(head, git(repo, "rev-parse", `${head}:a.rs`)) }],
			comments: [],
		}),
	);
	await until(() => !existsSync(batch), "the batch to be consumed");
	assert.equal(session.messages.length, 0);
	const state = JSON.parse(readFileSync(join(repo, ".pi", "helix-review", "state", "session-1.json"), "utf8"));
	assert.deepEqual(state.baseline, { commit_oid: head });
});
