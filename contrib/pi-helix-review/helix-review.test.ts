import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
	chmodSync,
	existsSync,
	mkdirSync,
	mkdtempSync,
	readFileSync,
	realpathSync,
	renameSync,
	rmSync,
	symlinkSync,
	unlinkSync,
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

function workspace(t: TestContext) {
	const root = mkdtempSync(join(tmpdir(), "pi-helix-review-"));
	t.after(() => rmSync(root, { recursive: true, force: true }));
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
function piSession(cwd: string) {
	const sent: { content: unknown; options: unknown }[] = [];
	const notes: { message: string; type?: string }[] = [];
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
		ui: { notify: (message: string, type?: string) => void notes.push({ message, type }) },
		isIdle: () => true,
		hasPendingMessages: () => false,
		sessionManager: { getSessionId: () => "session-1" },
	};
	const emit = async (type: string, event: object = {}) => {
		let result: unknown;
		for (const h of handlers.get(type) ?? []) result = (await h({ type, ...event }, ctx)) ?? result;
		return result;
	};
	return { sent, notes, messages, ctx, emit, run: (args: string) => handler!(args, ctx) };
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
	return new Promise<string[]>((done) => server.listen(path, () => done(lines)));
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
	const lines = await fakeHelix(t, join(runtime, "helix", `${id}.sock`));

	const file = join(repo, "a.rs");
	w.stage("a.rs", "fn a() {}\n");
	const head = w.commit();
	const outbox = join(repo, ".pi", "helix-review", "outbox");
	const state = () => JSON.parse(readFileSync(join(repo, ".pi", "helix-review", "state.json"), "utf8"));

	const session = piSession(join(repo));
	t.after(() => session.emit("session_shutdown"));
	await session.emit("session_start", { reason: "startup" });
	assert.ok(existsSync(outbox));
	assert.deepEqual(lines.splice(0), ["echo pi hook attached"], "nothing to restore without a baseline");
	assert.ok(existsSync(join(repo, ".pi", "helix-review", ".gitignore")));
	const promptGuidelines: string[] = [];
	await session.emit("before_agent_start", { prompt: "p", systemPrompt: "base", systemPromptOptions: { promptGuidelines } });
	assert.equal(promptGuidelines.length, 1);
	assert.match(promptGuidelines[0], /^## Helix review handoff\n.*AGENT:/s);
	assert.match(promptGuidelines[0], /steering message.*keep it undone.*new: false.*integrated or declined/s);

	// The fake Helix rejects `echo`, which must not fail the hook.
	const started = "echo Agent turn started";
	await session.emit("agent_start");
	assert.deepEqual(lines, [`review-handoff ${outbox}`, `review-baseline ${head}`, started]);
	assert.deepEqual(state().baseline, { commit_oid: head, acknowledged: false });

	writeFileSync(file, "fn a() {}\n// AGENT: old request\nfn b() {}\n");
	for (const toolName of ["edit", "write", "bash", "read"]) await session.emit("tool_result", { toolName, input: {}, content: [], isError: false });
	await until(() => lines.length === 4, "the coalesced refresh");
	await new Promise((wake) => setTimeout(wake, 300));
	assert.deepEqual(lines.slice(3), [`review-baseline ${head}`]);

	w.stage("a.rs", "fn a() {}\n// AGENT: old request\nfn b() {}\n");
	const next = w.commit();
	writeFileSync(join(repo, "new.rs"), "fn new() {}\n");
	// pi's own state is neither snapshotted nor counted as a changed file.
	const piState = join(repo, ".pi", "fabric", "state.json");
	mkdirSync(dirname(piState), { recursive: true });
	writeFileSync(piState, "{}\n");
	await session.emit("agent_end", { messages: [] });
	assert.deepEqual(lines.slice(4), [
		`review-baseline ${head}`,
		`echo Agent turn done: 2 files changed since ${head.slice(0, 8)} (:review-files)`,
	]);
	const snapshots = state().snapshots;
	assert.deepEqual(Object.keys(snapshots).sort(), [file, join(repo, "new.rs")]);
	assert.equal(snapshots[file], git(repo, "rev-parse", `${next}:a.rs`));

	await session.emit("agent_start");
	assert.deepEqual(lines.slice(6), [`review-handoff ${outbox}`, `review-baseline ${head}`, started], "the baseline is sticky");

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
	assert.deepEqual(handoff.agent_comments, [
		{ line: 2, text: "old request", new: false },
		{ line: 4, text: "rename b", new: true },
	]);
	assert.deepEqual(handoff.review_comments.map((c: { comment: string }) => c.comment), ["document a"]);
	assert.ok(message.content.includes(JSON.stringify(message.details, null, 2)), message.content);
	assert.match(message.content, /never revert them/);
	assert.equal(state().baseline.acknowledged, true);
	await until(() => lines.length === 10, "the receipt echo");
	assert.equal(lines[9], "echo pi received your review: 1 file with edits, 2 AGENT comments, 1 review comment");

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
	assert.equal(lines[10], "echo pi received your review: nothing to do");

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
	assert.equal(lines[12], "echo pi received your review: commit request");
	assert.equal(session.messages.length, 3);
	const commit = session.messages[2].message;
	assert.deepEqual(commit.details.files, []);
	assert.deepEqual(commit.details.request, { kind: "commit-staged", subject: "fix: capitalize b" });
	assert.match(commit.content, /User request:\nCommit exactly what is staged in the git index: run `git commit` without `-a`/);
	assert.match(commit.content, /Use this subject: "fix: capitalize b"\..*leave unstaged changes alone, and report the commit hash\./);
	requested("1-5.json", { kind: "message", text: "Please also update the changelog." });
	await until(() => lines.length === 14, "the message receipt");
	assert.equal(lines[13], "echo pi received your review: message");
	const instruction = session.messages[3].message;
	assert.deepEqual(instruction.details.request, { kind: "message", text: "Please also update the changelog." });
	assert.match(instruction.content, /User request:\nInstruction:\n```\nPlease also update the changelog\.\n```$/);

	await session.emit("agent_start");
	assert.deepEqual(lines.slice(14), [`review-handoff ${outbox}`, `review-baseline ${next}`, started], "an acknowledged baseline moves to HEAD");

	const restarted = piSession(repo);
	t.after(() => restarted.emit("session_shutdown"));
	await restarted.emit("session_start", { reason: "startup" });
	assert.deepEqual(
		lines.slice(17),
		[`review-handoff ${outbox}`, `review-baseline ${next}`, `echo pi hook attached, baseline ${next.slice(0, 8)}`],
		"session_start restores the baseline and says the link is live",
	);

	// A turn that leaves nothing changed since its (still sticky) baseline says so.
	writeFileSync(file, "fn a() {}\n// AGENT: old request\nfn b() {}\n");
	unlinkSync(join(repo, "new.rs"));
	await restarted.emit("agent_start");
	await restarted.emit("agent_end", { messages: [] });
	assert.deepEqual(lines.slice(20), [
		`review-handoff ${outbox}`,
		`review-baseline ${next}`,
		started,
		`review-baseline ${next}`,
		`echo Agent turn done: no file changes since ${next.slice(0, 8)}`,
	]);
});

test("an unreachable Helix never fails the hook, and an empty handoff is dropped but acknowledged", async (t) => {
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
	const state = JSON.parse(readFileSync(join(repo, ".pi", "helix-review", "state.json"), "utf8"));
	assert.deepEqual(state.baseline, { commit_oid: head, acknowledged: true });
});
