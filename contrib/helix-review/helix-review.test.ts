import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import fs from "node:fs/promises";
import { syncBuiltinESMExports } from "node:module";
import { join } from "node:path";
import { test, type TestContext } from "node:test";
import { BatchError, EmptyReviewError, ReviewBatch, buildHandoff } from "./index.ts";

const gitEnv = { ...process.env, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1" };
function workspace(t: TestContext) {
	const repo = mkdtempSync(join(tmpdir(), "helix-review-"));
	t.after(() => rmSync(repo, { recursive: true, force: true }));
	const git = (...args: string[]) => execFileSync("git", args, { cwd: repo, env: gitEnv, encoding: "utf8" }).trim();
	git("init", "-q");
	return {
		repo, git,
		stage(name: string, content: string) {
			writeFileSync(join(repo, name), content);
			git("add", "--", name);
			return git("rev-parse", `:${name}`);
		},
		commit() {
			git("-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-qm", "base");
			return git("rev-parse", "HEAD");
		},
	};
}
const index = (blob_oid: string) => ({ kind: "git-index", blob_oid });
const commit = (commit_oid: string, blob_oid: string | null) => ({ kind: "git-commit", commit_oid, blob_oid });
const doc = (files: unknown[] = [], comments: unknown[] = [], request?: unknown) => ({
	format: "helix-review-batch", version: 2, files, comments, ...(request ? { request } : {}),
});
function comment(path: string, baseline: unknown, overrides: Record<string, unknown> = {}) {
	return {
		path, baseline, side: "current", start_line: 1, end_line: 1, current_line: 1,
		selected_text: "selected", context_before: "", context_after: "\n", comment: "Please explain this.",
		...overrides,
	};
}
const parse = (value: unknown) => ReviewBatch.parse(JSON.stringify(value));

test("the v2 parser preserves exact snapshot text, baselines and requests while ignoring future fields", () => {
	const path = "/repo/a.rs";
	const base = commit("A".repeat(64), "B".repeat(64));
	const batch = parse({
		...doc([{ path, baseline: base }], [comment(path, { ...base, future: 7 }, {
			side: "old", selected_text: "old\r\n", context_before: "before\r\n", context_after: "after\r\n",
			enclosing_text: "function", comment: "Keep ``` this text.",
		})], { kind: "ask", ignored: true }), target: "t3-thread", future: true,
	});
	assert.deepEqual(batch.comments[0].baseline, { kind: "git-commit", commitOid: "a".repeat(64), blobOid: "b".repeat(64) });
	assert.equal(batch.comments[0].selectedText, "old\r\n");
	assert.equal(batch.comments[0].contextBefore, "before\r\n");
	assert.equal(batch.comments[0].contextAfter, "after\r\n");
	assert.equal(batch.comments[0].enclosingText, "function");
	assert.deepEqual(batch.request, { kind: "ask" });
	assert.equal(batch.target, "t3-thread");
});

test("invalid versions, old selections from absent files, invalid requests and entry limits are rejected", () => {
	const file = { path: "/repo/a", baseline: index("a".repeat(40)) };
	const cases: [unknown, RegExp][] = [
		[{ ...doc([file]), version: 1 }, /unsupported version 1/],
		[doc([file], [comment(file.path, commit("a".repeat(40), null), { side: "old" })]), /cannot select old text from an absent file/],
		[doc([file], [], { kind: "shell", text: "ignored" }), /request.kind/],
		[doc([file], [], { kind: "message", text: 3 }), /request.text must be a string/],
		[doc([file], [], { kind: "pr-review", summary: 3 }), /request.summary must be a string/],
		[doc([file, file]), /more than once/],
		[doc(Array(1001).fill(file)), /more than 1000/],
		[doc([], [comment(file.path, file.baseline, { start_line: 0 })]), /1-based line number/],
		[doc([{ ...file, path: "relative" }]), /absolute path/],
	];
	for (const [value, message] of cases) assert.throws(() => parse(value), (error) => error instanceof BatchError && message.test(error.message));
});

test("batch reads reject oversized files and invalid UTF-8 before producing a handoff", async (t) => {
	const { repo } = workspace(t);
	const path = join(repo, "batch.json");
	writeFileSync(path, Buffer.from([0x7b, 0xff, 0x7d]));
	await assert.rejects(ReviewBatch.read(path), /not valid UTF-8/);
	writeFileSync(path, " ".repeat(4 * 1024 * 1024 + 1));
	await assert.rejects(ReviewBatch.read(path), /larger than 4 MiB/);
	await assert.rejects(ReviewBatch.read(repo), /not a regular file/);
});

test("inspection warns about moved index and current text while retaining authoritative old selections", async (t) => {
	const w = workspace(t);
	const path = join(w.repo, "a.rs");
	const oid = w.stage("a.rs", "before\nremoved\nafter\n");
	const baseline = index(oid);
	const batch = parse(doc([{ path, baseline }], [
		comment(path, baseline, { side: "old", start_line: 2, end_line: 2, current_line: 2, selected_text: "removed\n", context_before: "before\n", context_after: "after\n" }),
		comment(path, baseline, { selected_text: "vanished" }),
		comment(path, baseline, { selected_text: "selected", context_after: "unmatched" }),
	]));
	w.stage("a.rs", "restaged\n");
	writeFileSync(path, "prefix\nselected\n");
	const { message, warnings } = batch.render("batch.json", await batch.inspect());
	assert.match(warnings.join("\n"), /Git index changed after export/);
	assert.match(warnings.join("\n"), /changed since the comment/);
	assert.match(warnings.join("\n"), /selected text was not found/);
	assert.match(warnings.join("\n"), /selected text alone appears at line 2/);
	assert.match(message, /Selected text:\n```\nremoved\n```/);
	assert.match(message, /as Helix displayed it after worktree conversions/);
});

test("immutable commit baselines survive HEAD and index moves, but inconsistent metadata is warned about", async (t) => {
	const w = workspace(t);
	const path = join(w.repo, "a.rs");
	const old = w.stage("a.rs", "selected\n");
	const head = w.commit();
	const batch = parse(doc([{ path, baseline: commit(head, old) }], [comment(path, commit(head, old))]));
	const moved = w.stage("a.rs", "new index\n");
	w.commit();
	writeFileSync(path, "selected\n");
	assert.deepEqual(batch.render("batch.json", await batch.inspect()).warnings, []);
	const invalid = parse(doc([{ path, baseline: commit(head, moved) }]));
	assert.match(invalid.render("batch.json", await invalid.inspect()).warnings.join("\n"), /but the batch says blob/);
});

test("handoffs carry user corrections since the turn snapshot and both selection sides with their baselines", async (t) => {
	const w = workspace(t);
	const path = join(w.repo, "a.rs");
	const blob = w.stage("a.rs", "original\nremoved\n");
	const head = w.commit();
	const snapshot = w.stage("a.rs", "agent change\nremoved\n");
	writeFileSync(path, "user correction\n");
	const comments = [
		comment(path, commit(head, blob), { selected_text: "user correction\n", context_after: "" }),
		comment(path, index(snapshot), { side: "old", start_line: 2, end_line: 2, current_line: 2, selected_text: "removed\n", context_after: "", comment: "Why removed?" }),
	];
	const batch = parse(doc([{ path, baseline: commit(head, blob) }], comments));
	const before = readFileSync(path);
	const staged = w.git("rev-parse", ":a.rs");
	const handoff = await buildHandoff(batch, { repo: w.repo, baseline: head, snapshots: { [path]: snapshot } });
	assert.ok(handoff);
	assert.equal(handoff.details.baseline, head);
	assert.equal(handoff.files[0].user_edits.against, "turn-snapshot");
	assert.match(handoff.files[0].user_edits.hunks.join(""), /-agent change\n.*\+user correction/s);
	assert.deepEqual(handoff.files[0].review_comments.map((c) => c.baseline), [commit(head, blob), index(snapshot)]);
	assert.deepEqual(handoff.files[0].review_comments.map((c) => c.side), ["current", "old"]);
	assert.match(handoff.content, /never revert them/);
	assert.deepEqual(readFileSync(path), before);
	assert.equal(w.git("rev-parse", ":a.rs"), staged);
	assert.equal(w.git("rev-parse", "HEAD"), head);
	const pi = await buildHandoff(batch, { repo: w.repo, includeCommentBaselines: false });
	assert.ok(pi!.files[0].review_comments.every((c) => !Object.hasOwn(c, "baseline")));
});

test("request-only handoffs preserve fenced text and spell out staged commit constraints", async (t) => {
	const { repo } = workspace(t);
	const text = "Please explain ```sh\n$(example)\n``` without changes.";
	const handoff = await buildHandoff(parse(doc([], [], { kind: "message", text })), { repo });
	assert.ok(handoff);
	assert.deepEqual(handoff.details.request, { kind: "message", text });
	assert.ok(handoff.content.endsWith(`User request:\nInstruction:\n\`\`\`\`\n${text}\n\`\`\`\``));
	const staged = await buildHandoff(parse(doc([], [], { kind: "commit-staged", subject: "fix: correction" })), { repo });
	assert.match(staged!.content, /git commit` without `-a` and without staging anything first/);
	assert.match(staged!.content, /Use this subject: "fix: correction"/);
	assert.match(staged!.content, /leave unstaged changes alone, and report the commit hash/);
});

test("pull request reviews diff saved files against HEAD without context and ask for inline comments and suggestions", async (t) => {
	const w = workspace(t);
	const path = join(w.repo, "a.rs");
	const added = join(w.repo, "new.rs");
	const mergeBase = w.stage("a.rs", "zero\n");
	const base = w.commit();
	w.stage("a.rs", "one\ntwo\nthree\nfour\nfive\nsix\nseven\n");
	const head = w.commit();
	writeFileSync(path, "one\nTWO\nthree\nfour\nfive\nsix\nSEVEN\n");
	writeFileSync(added, "fresh\n");
	const files = [{ path, baseline: commit(base, mergeBase) }, { path: added, baseline: commit(base, null) }];
	const note = comment(path, commit(base, mergeBase), {
		start_line: 2, end_line: 2, current_line: 2, selected_text: "TWO\n", context_before: "one\n", context_after: "three\n", comment: "Name this better.",
	});
	const batch = parse(doc(files, [note], { kind: "pr-review", summary: "Close, two nits." }));
	assert.deepEqual(batch.request, { kind: "pr-review", summary: "Close, two nits." });
	const before = readFileSync(path);
	const staged = w.git("rev-parse", ":a.rs");
	// The turn snapshot and the comment baseline are not the pull request's lines; HEAD is.
	const handoff = await buildHandoff(batch, { repo: w.repo, snapshots: { [path]: mergeBase } });
	assert.ok(handoff);
	assert.deepEqual(handoff.files.map((file) => [file.path, file.user_edits.against]), [[path, "head"], [added, "head"]]);
	assert.match(handoff.files[0].user_edits.hunks.join(""), /^@@ -2 \+2 @@.*\n-two\n\+TWO\n@@ -7 \+7 @@.*\n-seven\n\+SEVEN\n$/);
	assert.match(handoff.files[1].user_edits.hunks.join(""), /^@@ -0,0 \+1 @@.*\n\+fresh\n$/);
	assert.equal(handoff.files[0].review_comments[0].comment, "Name this better.");
	assert.deepEqual(handoff.details.request, { kind: "pr-review", summary: "Close, two nits." });
	assert.match(handoff.content, /head commit is the local HEAD/);
	assert.match(handoff.content, /```suggestion fence/);
	assert.match(handoff.content, /do not commit, stage, push or revert them/);
	assert.ok(!handoff.content.includes("continue editing or committing normally"));
	assert.ok(handoff.content.includes("Review body:\n```\nClose, two nits.\n```"));
	assert.deepEqual(readFileSync(path), before);
	assert.equal(w.git("rev-parse", ":a.rs"), staged);
	assert.equal(w.git("rev-parse", "HEAD"), head);
	const unsummarized = await buildHandoff(parse(doc(files, [], { kind: "pr-review" })), { repo: w.repo });
	assert.deepEqual(unsummarized!.details.request, { kind: "pr-review" });
	assert.match(unsummarized!.content, /write a short summary/);
});

test("pull request reviews read HEAD from the repository root after a deletion removed the file's directory", async (t) => {
	const w = workspace(t);
	mkdirSync(join(w.repo, "gone", "deeper"), { recursive: true });
	const path = join(w.repo, "gone", "deeper", "a.rs");
	const blob = w.stage("gone/deeper/a.rs", "removed\n");
	const head = w.commit();
	rmSync(join(w.repo, "gone"), { recursive: true });
	const batch = parse(doc([{ path, baseline: commit(head, blob) }], [], { kind: "pr-review" }));
	assert.deepEqual(batch.render("batch.json", await batch.inspect(w.repo)).warnings, [`${JSON.stringify(path)} is missing on disk.`]);
	const handoff = await buildHandoff(batch, { repo: w.repo, strictCurrentReads: true });
	assert.equal(handoff!.files[0].user_edits.against, "head");
	assert.match(handoff!.files[0].user_edits.hunks.join(""), /^@@ -1 \+0,0 @@.*\n-removed\n$/);
});

test("an empty pull request review is refused, but a summary alone is a review", async (t) => {
	const w = workspace(t);
	const path = join(w.repo, "a.rs");
	const blob = w.stage("a.rs", "unchanged\n");
	const head = w.commit();
	const files = [{ path, baseline: commit(head, blob) }];
	await assert.rejects(
		buildHandoff(parse(doc(files, [], { kind: "pr-review" })), { repo: w.repo }),
		(error) => error instanceof EmptyReviewError && /no comments, no edits against HEAD and no summary/.test(error.message),
	);
	const summarized = await buildHandoff(parse(doc(files, [], { kind: "pr-review", summary: "Looks good." })), { repo: w.repo });
	assert.deepEqual(summarized!.files, []);
	assert.deepEqual(summarized!.details.request, { kind: "pr-review", summary: "Looks good." });
});

test("ask handoffs use relocated selections and nearest hunks, excluding distant edits, other files and enclosing code", async (t) => {
	const w = workspace(t);
	const path = join(w.repo, "a.rs");
	const other = join(w.repo, "other.rs");
	const base = Array.from({ length: 100 }, (_, i) => `original-${i + 1}\n`);
	const blob = w.stage("a.rs", base.join(""));
	const otherBlob = w.stage("other.rs", "other base\n");
	// Review context must remain local even when the repository requests expansive diffs.
	w.git("config", "diff.context", "500");
	w.git("config", "diff.interHunkContext", "500");
	const head = w.commit();
	const current = [...Array.from({ length: 20 }, (_, i) => `PREFIX_${i}\n`), ...base];
	current[29] = "AGENT_NEAR\n";
	current[79] = "AGENT_FAR\n";
	const snapshot = w.stage("a.rs", current.join(""));
	current[29] = "USER_NEAR\n";
	current[79] = "USER_FAR\n";
	writeFileSync(path, current.join(""));
	writeFileSync(other, "USER_OTHER\n");
	const files = [{ path, baseline: commit(head, blob) }, { path: other, baseline: commit(head, otherBlob) }];
	for (const side of ["current", "old"]) {
		const selection = comment(path, commit(head, blob), {
			side, start_line: 10, end_line: 10, current_line: 10,
			selected_text: side === "old" ? "original-10\n" : "USER_NEAR\n",
			context_before: "original-9\n", context_after: "original-11\n", enclosing_text: "ENTIRE_FUNCTION",
		});
		const handoff = await buildHandoff(parse(doc(files, [selection], { kind: "ask" })), { repo: w.repo, snapshots: { [path]: snapshot } });
		assert.deepEqual(handoff!.files.map((f) => f.path), [path]);
		assert.match(handoff!.files[0].review_comments[0].context_hunks!.hunks.join(""), /USER_NEAR/);
		assert.match(handoff!.files[0].user_edits.hunks.join(""), /AGENT_NEAR[\s\S]*USER_NEAR/);
		for (const unrelated of ["PREFIX_", "USER_FAR", "AGENT_FAR", "USER_OTHER", "ENTIRE_FUNCTION", other]) assert.ok(!handoff!.content.includes(unrelated), unrelated);
		assert.equal(handoff!.files[0].review_comments[0].enclosing_text, undefined);
		assert.match(handoff!.content, /Do what the message asks/);
	}
	const full = await buildHandoff(parse(doc(files, [], { kind: "message", text: "Review everything" })), { repo: w.repo });
	assert.match(full!.content, /USER_FAR/);
	assert.match(full!.content, /USER_OTHER/);
});

test("whole hunks are capped at 200 lines and empty or excluded files create no handoff", async (t) => {
	const w = workspace(t);
	const path = join(w.repo, "a.rs");
	const blob = w.stage("a.rs", "base\n");
	const batch = parse(doc([{ path, baseline: index(blob) }]));
	assert.equal(await buildHandoff(batch, { repo: w.repo }), undefined);
	writeFileSync(path, "base\n" + "extra\n".repeat(300));
	const handoff = await buildHandoff(batch, { repo: w.repo });
	assert.equal(handoff!.files[0].user_edits.truncated, true);
	assert.match(handoff!.files[0].user_edits.hunks[0], /^\.\.\. 1 more hunk/);
	assert.equal(await buildHandoff(batch, { repo: w.repo, excludePath: () => true }), undefined);
	assert.ok(existsSync(path));
});

test("strict current reads reject oversized and non-regular saved files while accepting actual deletion", async (t) => {
	const w = workspace(t);
	const path = join(w.repo, "a.rs");
	const blob = w.stage("a.rs", "selected\n");
	const batch = parse(doc([{ path, baseline: index(blob) }]));
	await fs.writeFile(path, Buffer.alloc(8 * 1024 * 1024 + 1, 0x78));
	await assert.rejects(buildHandoff(batch, { repo: w.repo, strictCurrentReads: true }), /larger than 8 MiB.*No deletion has been inferred/);
	await fs.rm(path);
	await fs.mkdir(path);
	await assert.rejects(buildHandoff(batch, { repo: w.repo, strictCurrentReads: true }), /not a regular file.*No deletion has been inferred/);
	await fs.rm(path, { recursive: true });
	const deleted = await buildHandoff(batch, { repo: w.repo, strictCurrentReads: true });
	assert.match(deleted!.files[0].user_edits.hunks.join(""), /-selected\n/);
});

test("strict reads reject permission errors without manufacturing removed text", async (t) => {
	const w = workspace(t);
	const path = join(w.repo, "a.rs");
	const blob = w.stage("a.rs", "selected\n");
	const batch = parse(doc([{ path, baseline: index(blob) }]));
	const read = fs.readFile;
	const probe = t.mock.method(fs, "readFile", async (...args: Parameters<typeof read>) => {
		if (args[0] === path) throw Object.assign(new Error("test permission denied"), { code: "EACCES" });
		return read(...args);
	});
	syncBuiltinESMExports();
	t.after(() => { probe.mock.restore(); syncBuiltinESMExports(); });
	await assert.rejects(buildHandoff(batch, { repo: w.repo, strictCurrentReads: true }), /permission denied.*No deletion has been inferred/);
});

test("strict reads enforce the size bound when a file grows after its stat check", async (t) => {
	const w = workspace(t);
	const path = join(w.repo, "a.rs");
	const blob = w.stage("a.rs", "selected\n");
	const batch = parse(doc([{ path, baseline: index(blob) }]));
	const read = fs.readFile;
	const probe = t.mock.method(fs, "readFile", async (...args: Parameters<typeof read>) => {
		if (args[0] === path) await fs.writeFile(path, Buffer.alloc(8 * 1024 * 1024 + 1, 0x78));
		return read(...args);
	});
	syncBuiltinESMExports();
	t.after(() => { probe.mock.restore(); syncBuiltinESMExports(); });
	await assert.rejects(buildHandoff(batch, { repo: w.repo, strictCurrentReads: true }), /larger than 8 MiB.*No deletion has been inferred/);
});
