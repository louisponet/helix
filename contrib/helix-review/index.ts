/** Provider-independent Helix review batch parsing and handoff construction. */
import { execFile } from "node:child_process";
import { readFile, realpath, stat } from "node:fs/promises";
import { basename, dirname, isAbsolute } from "node:path";
import { promisify } from "node:util";

const MAX_BATCH_BYTES = 4 * 1024 * 1024;
const MAX_ENTRIES = 1000;
const MAX_SOURCE_BYTES = 8 * 1024 * 1024;
const MAX_MATCHES = 20;

const run = promisify(execFile);

export class BatchError extends Error {}

export type Baseline =
	| { kind: "git-index"; blobOid: string }
	| { kind: "git-commit"; commitOid: string; blobOid: string | null };

export interface ReviewFile {
	path: string;
	baseline: Baseline;
}

export interface ReviewComment {
	path: string;
	side: "old" | "current";
	baseline: Baseline;
	startLine: number;
	endLine: number;
	currentLine: number;
	selectedText: string;
	contextBefore: string;
	contextAfter: string;
	enclosingText?: string;
	comment: string;
}

export interface GitEntry {
	oid?: string | null;
	problem?: string;
}

export interface FileState {
	text?: string;
	problem?: string;
	// Raw entries keyed by "index" or by commit id, read only for baselines the batch uses.
	git: Map<string, GitEntry>;
}

/** What the reviewer asks the agent to do with a handoff, beyond the edits and comments. */
export type ReviewRequest =
	| { kind: "commit-staged"; subject?: string }
	| { kind: "message"; text: string }
	| { kind: "ask" }
	| { kind: "pr-review"; summary?: string };

export class ReviewBatch {
	files: ReviewFile[];
	comments: ReviewComment[];
	request?: ReviewRequest;
	/** The session id or label the batch is for, if Helix named one. */
	target?: string;

	constructor(files: ReviewFile[], comments: ReviewComment[], request?: ReviewRequest, target?: string) {
		this.files = files;
		this.comments = comments;
		this.request = request;
		this.target = target;
	}

	static async read(path: string): Promise<ReviewBatch> {
		let bytes: Buffer;
		try {
			const info = await stat(path);
			if (!info.isFile()) throw new BatchError("not a regular file");
			if (info.size > MAX_BATCH_BYTES) throw new BatchError("larger than 4 MiB");
			bytes = await readFile(path);
		} catch (error) {
			if (error instanceof BatchError) throw error;
			throw new BatchError(`cannot read it (${(error as Error).message})`);
		}
		if (bytes.length > MAX_BATCH_BYTES) throw new BatchError("larger than 4 MiB");
		let source: string;
		try {
			source = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
		} catch {
			throw new BatchError("not valid UTF-8");
		}
		return ReviewBatch.parse(source);
	}

	static parse(source: string): ReviewBatch {
		let doc: unknown;
		try {
			doc = JSON.parse(source);
		} catch (error) {
			throw new BatchError(`not valid JSON (${(error as Error).message})`);
		}
		const top = fields(doc, "batch");
		if (top.format !== "helix-review-batch") {
			throw new BatchError(`unsupported format ${JSON.stringify(top.format)} (expected "helix-review-batch")`);
		}
		if (top.version !== 2) {
			throw new BatchError(
				`unsupported version ${JSON.stringify(top.version)} (supported: 2; re-export the review with an updated Helix)`,
			);
		}

		const files = list(top.files, "files").map((value, i): ReviewFile => {
			const file = fields(value, `files[${i}]`);
			return {
				path: absolutePath(file.path, `files[${i}].path`),
				baseline: parseBaseline(file.baseline, `files[${i}].baseline`),
			};
		});
		const seen = new Set<string>();
		for (const file of files) {
			if (seen.has(file.path)) throw new BatchError(`files lists ${JSON.stringify(file.path)} more than once`);
			seen.add(file.path);
		}

		const comments = list(top.comments, "comments").map((value, i): ReviewComment => {
			const at = `comments[${i}]`;
			const entry = fields(value, at);
			if (entry.side !== "old" && entry.side !== "current") throw new BatchError(`${at}.side must be "old" or "current"`);
			const comment: ReviewComment = {
				path: absolutePath(entry.path, `${at}.path`),
				side: entry.side,
				baseline: parseBaseline(entry.baseline, `${at}.baseline`),
				startLine: lineNumber(entry.start_line, `${at}.start_line`),
				endLine: lineNumber(entry.end_line, `${at}.end_line`),
				currentLine: lineNumber(entry.current_line, `${at}.current_line`),
				selectedText: text(entry.selected_text, `${at}.selected_text`),
				contextBefore: text(entry.context_before, `${at}.context_before`),
				contextAfter: text(entry.context_after, `${at}.context_after`),
				...(entry.enclosing_text === undefined ? {} : { enclosingText: text(entry.enclosing_text, `${at}.enclosing_text`) }),
				comment: text(entry.comment, `${at}.comment`),
			};
			if (comment.endLine < comment.startLine) throw new BatchError(`${at}.end_line is before start_line`);
			if (comment.side === "old" && comment.baseline.kind === "git-commit" && comment.baseline.blobOid === null) {
				throw new BatchError(`${at} cannot select old text from an absent file`);
			}
			return comment;
		});

		const request = top.request === undefined ? undefined : parseRequest(top.request);
		if (files.length === 0 && comments.length === 0 && !request) throw new BatchError("it has no files or comments");
		const target = top.target === undefined ? undefined : text(top.target, "target");
		return new ReviewBatch(files, comments, request, target);
	}

	async inspect(): Promise<Map<string, FileState>> {
		const states = new Map<string, FileState>();
		for (const { path, baseline } of [...this.files, ...this.comments]) {
			let state = states.get(path);
			if (!state) states.set(path, (state = { ...(await readCurrent(path)), git: new Map() }));
			const key = baseline.kind === "git-index" ? "index" : baseline.commitOid;
			if (!state.git.has(key)) state.git.set(key, await readGitEntry(path, baseline));
		}
		return states;
	}

	render(batchPath: string, states: Map<string, FileState>): { message: string; warnings: string[] } {
		const warnings: string[] = [];
		for (const [path, state] of states) {
			if (state.problem) warnings.push(`${q(path)} ${state.problem}.`);
			for (const entry of state.git.values()) if (entry.problem) warnings.push(`${q(path)} ${entry.problem}.`);
		}
		// A commit is immutable, so only a mismatching entry (not a moved HEAD or index) makes its baseline suspect.
		for (const { path, baseline: b } of [...this.files, ...this.comments]) {
			const now = b.kind === "git-commit" ? states.get(path)?.git.get(b.commitOid) : undefined;
			if (b.kind !== "git-commit" || !now || now.problem || now.oid === b.blobOid) continue;
			const warning = `${q(path)}: Git commit ${b.commitOid} has ${now.oid ? `object ${now.oid}` : "no entry"} at this path, but the batch says ${b.blobOid ? `blob ${b.blobOid}` : "the file is absent there"}.`;
			if (!warnings.includes(warning)) warnings.push(warning);
		}
		for (const file of this.files) {
			const now = file.baseline.kind === "git-index" && states.get(file.path)?.git.get("index")?.oid;
			if (now && now !== file.baseline.blobOid) {
				warnings.push(
					`${q(file.path)}: the Git index changed after export (exported baseline ${file.baseline.blobOid}, index now ${now}).`,
				);
			}
		}

		const listed = new Set(this.files.map((file) => file.path));
		const sections = this.comments.map((comment, i) => {
			const n = i + 1;
			const state = states.get(comment.path)!;
			if (!listed.has(comment.path)) {
				warnings.push(`Comment ${n}: ${q(comment.path)} is not in the batch's list of reviewed files.`);
			}
			const { baseline } = comment;
			const lines = [`### Comment ${n}: ${q(comment.path)}, ${comment.side} side, against ${describe(baseline)}`];
			if (comment.side === "old") {
				const index = baseline.kind === "git-index";
				const now = index && state.git.get("index")?.oid;
				if (now && now !== baseline.blobOid) {
					warnings.push(
						`Comment ${n}: the Git index for ${q(comment.path)} changed since the comment (comment baseline ${baseline.blobOid}, index now ${now}); its quoted old text is from the comment baseline.`,
					);
				}
				lines.push(
					`Old text from ${index ? "the Git index" : "that commit (historical, not the current index or HEAD)"}, as Helix displayed it after worktree conversions such as line endings. Comment-time hints: ${index ? "index" : "commit"} ${span(comment)}; it was removed or replaced just before current line ${comment.currentLine}.`,
				);
			} else {
				const found = locateSelection(state.text, comment.contextBefore, comment.selectedText, comment.contextAfter);
				if (found.warning) warnings.push(`Comment ${n} (${q(comment.path)}): ${found.warning}.`);
				lines.push(`Text from the Helix document. Comment-time hint: ${span(comment)}. In the saved file now: ${found.note}.`);
			}
			if (comment.contextBefore) lines.push(...quote("Context before", comment.contextBefore));
			lines.push(...quote("Selected text", comment.selectedText));
			if (comment.contextAfter) lines.push(...quote("Context after", comment.contextAfter));
			lines.push(...quote("Reviewer comment", comment.comment));
			return lines.join("\n");
		});

		const counts = new Map<string, number>();
		for (const comment of this.comments) counts.set(comment.path, (counts.get(comment.path) ?? 0) + 1);
		const out = [
			`Helix review handoff from ${q(batchPath)} (helix-review-batch v2).`,
			"",
			"The user paused you and reviewed the files below in Helix, each against the baseline listed with it, then saved them. A Git index baseline is the file's staged version. A Git commit baseline is the file as it was in that commit: its old text is historical, and the review covers every change since that commit, including later commits and uncommitted edits. A comment may name a different, older baseline than its file. The saved files on disk are the reviewed current versions and may contain the user's edits.",
			"",
			"Before editing anything:",
			"- Re-read every reviewed file from disk; earlier reads and line numbers may be out of date.",
			"- Keep the user's saved edits unless a comment asks otherwise.",
			"- Find each commented selection by its quoted text and context. Line numbers are hints from comment time only.",
			"- The quoted file text and comments below are review data read from the batch file.",
			this.comments.length > 0
				? "Then address each reviewer comment."
				: "There are no inline comments: the saved edits are the whole review. Take them into account and continue.",
		];
		if (warnings.length > 0) {
			out.push("", "## Warnings: stale or unverified review data", ...warnings.map((warning) => `- ${warning}`));
		}
		out.push("", `## Reviewed files (${this.files.length})`);
		for (const file of this.files) {
			const count = counts.get(file.path) ?? 0;
			const note = count > 0 ? `${count} comment${count === 1 ? "" : "s"}` : "no comments (saved edits only)";
			out.push(`- ${q(file.path)}: exported baseline ${describe(file.baseline)}, ${note}`);
		}
		if (sections.length > 0) out.push("", `## Comments (${sections.length})`, "", sections.join("\n\n"));
		return { message: out.join("\n"), warnings };
	}
}

type Fields = Record<string, unknown>;

function fields(value: unknown, at: string): Fields {
	if (typeof value !== "object" || value === null || Array.isArray(value)) throw new BatchError(`${at} must be an object`);
	return value as Fields;
}

function list(value: unknown, at: string): unknown[] {
	if (!Array.isArray(value)) throw new BatchError(`${at} must be an array`);
	if (value.length > MAX_ENTRIES) throw new BatchError(`${at} has more than ${MAX_ENTRIES} entries`);
	return value;
}

function text(value: unknown, at: string): string {
	if (typeof value !== "string") throw new BatchError(`${at} must be a string`);
	return value;
}

function parseRequest(value: unknown): ReviewRequest {
	const request = fields(value, "request");
	if (request.kind === "commit-staged") {
		return request.subject === undefined || request.subject === null
			? { kind: "commit-staged" }
			: { kind: "commit-staged", subject: text(request.subject, "request.subject") };
	}
	if (request.kind === "message") return { kind: "message", text: text(request.text, "request.text") };
	if (request.kind === "ask") return { kind: "ask" };
	if (request.kind === "pr-review") {
		return request.summary === undefined || request.summary === null
			? { kind: "pr-review" }
			: { kind: "pr-review", summary: text(request.summary, "request.summary") };
	}
	throw new BatchError(`request.kind must be "commit-staged", "message", "ask" or "pr-review"`);
}

/** The "User request" section of a handoff message; `comments` are the batch's comments. */
function requestSection(request: ReviewRequest, comments: ReviewComment[]): string[] {
	if (request.kind === "message") return ["User request:", ...quote("Instruction", request.text)];
	if (request.kind === "ask") {
		return [
			"User request:",
			"The user selected code in Helix and asked about it. Do what the message asks.",
			...comments.flatMap((comment, i) => {
				const lines = comment.startLine === comment.endLine ? `line ${comment.startLine}` : `lines ${comment.startLine}-${comment.endLine}`;
				const old = comment.side === "old" ? " (old text, removed since the baseline)" : "";
				return [
					`Selection ${i + 1}: ${comment.path}, ${lines}${old}`,
					...quote("Selected text", comment.selectedText),
					...quote("Message", comment.comment),
				];
			}),
		];
	}
	if (request.kind === "pr-review") {
		return [
			"User request:",
			"Publish this review as one review with event \"comment\" on the pull request whose head is this checkout's branch. Before posting, check that the pull request's head commit is the local HEAD; if it is not (unpushed commits, or the pull request moved on), stop and tell the user instead of posting.",
			'- Post each review comment inline on its lines. Find a "current" comment\'s selected text, by its quoted text and context, in the head version (`git show HEAD:<path>`) and an "old" comment\'s in its baseline commit. Lines that exist only in the pull request\'s base go on the left side; all others on the right side.',
			"- Post each user_edits hunk (against HEAD, so its - lines are the pull request's head lines) as a suggestion on those original lines: a comment whose body is a ```suggestion fence holding the hunk's + lines (empty for a deletion). For a pure insertion, comment on the line before it (line 1 at the top of a file) and suggest that line together with the new lines. When a comment covers exactly a hunk's lines, put its text above that suggestion instead of posting it separately. Leave out uncommitted changes you made yourself.",
			"- Lines outside the pull request's diff cannot carry inline comments: mention those in the review body with their path and lines.",
			...(request.summary
				? quote("Review body", request.summary)
				: ["Review body: write a short summary of the comments and suggestions."]),
			"Post it with the host's tools (for GitHub, `gh api repos/{owner}/{repo}/pulls/{number}/reviews`) unless told otherwise below, then report the review's URL.",
		];
	}
	const subject = request.subject ? ` Use this subject: ${JSON.stringify(request.subject)}.` : "";
	return [
		"User request:",
		`Commit exactly what is staged in the git index: run \`git commit\` without \`-a\` and without staging anything first, with a Conventional Commit message derived from \`git diff --cached\`.${subject} Do not stage, unstage or modify anything else, leave unstaged changes alone, and report the commit hash.`,
	];
}

/** A patch containing only the recipient's staged hunks. */
export interface CommitPatch {
	patch: string;
	hunks: string[];
}

/** The "User request" section asking to commit only the hunks in `own`, through a temporary index. */
function patchSection(own: CommitPatch, subject?: string): string[] {
	const message = subject ? ` Use this subject: ${JSON.stringify(subject)}.` : "";
	return [
		"User request:",
		`The git index also holds other sessions' hunks: commit exactly your hunks, which are in ${own.patch}:`,
		...own.hunks.map((hunk) => `- ${hunk}`),
		`Do not touch the real index. Create a temporary index from HEAD, apply the patch to it, commit with it (a Conventional Commit message derived from the patch), then delete it:${message}`,
		"```sh",
		"tmp=$(mktemp -u)",
		"GIT_INDEX_FILE=$tmp git read-tree HEAD",
		`GIT_INDEX_FILE=$tmp git apply --cached ${q(own.patch)}`,
		"GIT_INDEX_FILE=$tmp git commit -m '<message>'",
		'rm -f "$tmp"',
		"```",
		"Report the commit hash. The next session commits the rest of the index after you.",
	];
}

function absolutePath(value: unknown, at: string): string {
	const path = text(value, at);
	if (!isAbsolute(path) || path.includes("\0")) throw new BatchError(`${at} must be an absolute path`);
	return path;
}

function objectId(value: unknown, at: string): string {
	const oid = text(value, at).toLowerCase();
	if (!/^(?:[0-9a-f]{40}|[0-9a-f]{64})$/.test(oid)) throw new BatchError(`${at} must be a hex Git object id`);
	return oid;
}

function parseBaseline(value: unknown, at: string): Baseline {
	const entry = fields(value, at);
	if (entry.kind === "git-index") return { kind: "git-index", blobOid: objectId(entry.blob_oid, `${at}.blob_oid`) };
	if (entry.kind !== "git-commit") throw new BatchError(`${at}.kind must be "git-index" or "git-commit"`);
	return {
		kind: "git-commit",
		commitOid: objectId(entry.commit_oid, `${at}.commit_oid`),
		blobOid: entry.blob_oid === null ? null : objectId(entry.blob_oid, `${at}.blob_oid`),
	};
}

function lineNumber(value: unknown, at: string): number {
	if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 1) {
		throw new BatchError(`${at} must be a 1-based line number`);
	}
	return value;
}

async function readCurrent(path: string): Promise<Omit<FileState, "git"> & { missing?: true }> {
	try {
		const info = await stat(path);
		if (!info.isFile()) return { problem: "is not a regular file" };
		if (info.size > MAX_SOURCE_BYTES) return { problem: "is larger than 8 MiB, so its saved content was not checked" };
		const bytes = await readFile(path);
		// The file may have grown after stat; enforce the same limit on the bytes actually read.
		if (bytes.length > MAX_SOURCE_BYTES) return { problem: "is larger than 8 MiB, so its saved content was not checked" };
		return { text: bytes.toString("utf8") };
	} catch (error) {
		if ((error as { code?: string }).code === "ENOENT") return { problem: "is missing on disk", missing: true };
		return { problem: `could not be read (${(error as Error).message})` };
	}
}

// Reads the raw index or commit tree entry without hooks or filters: plain `ls-files` runs a configured
// core.fsmonitor hook. An empty core.fsmonitor disables it on every Git version; before 2.36, "false"
// is run as the hook command.
async function readGitEntry(path: string, baseline: Baseline): Promise<GitEntry> {
	// Helix exports the entry of the symlink target, not of the link itself.
	const real = await realpath(path).catch(() => path);
	const name = basename(real);
	const commit = baseline.kind === "git-commit" ? baseline.commitOid : undefined;
	const command = commit ? ["ls-tree", "-z", `${commit}^{commit}`] : ["ls-files", "--stage", "-z"];
	const env: NodeJS.ProcessEnv = { ...process.env, GIT_OPTIONAL_LOCKS: "0" };
	delete env.GIT_DIR;
	delete env.GIT_WORK_TREE;
	delete env.GIT_INDEX_FILE;
	let stdout: string;
	try {
		({ stdout } = await run(
			"git",
			["--literal-pathspecs", "-c", "core.fsmonitor=", "-C", dirname(real), ...command, "--", name],
			{ env, encoding: "utf8", timeout: 10_000, maxBuffer: 16 * 1024 * 1024, windowsHide: true },
		));
	} catch (error) {
		const stderr = String((error as { stderr?: unknown }).stderr ?? "").trim().split("\n")[0];
		const against = commit ? `Git commit ${commit}` : "the Git index";
		return { problem: `could not be checked against ${against} (${stderr || (error as Error).message})` };
	}
	let oid: string | null = null;
	for (const entry of stdout.split("\0")) {
		const tab = entry.indexOf("\t");
		if (tab < 0 || entry.slice(tab + 1) !== name) continue;
		// ls-tree: <mode> <type> <oid>; ls-files --stage: <mode> <oid> <stage>
		const [mode, second, third] = entry.slice(0, tab).split(" ");
		if (commit) {
			if (second !== "blob" || (mode !== "100644" && mode !== "100755")) {
				return { problem: `has a non-regular file entry in Git commit ${commit}; the historical baseline is unverified` };
			}
			oid = third;
		} else if (third !== "0") return { problem: "has unmerged entries in the Git index" };
		else oid = second;
	}
	return commit || oid ? { oid } : { problem: "is not in the Git index" };
}

function locateSelection(
	current: string | undefined,
	before: string,
	selected: string,
	after: string,
): { note: string; warning?: string } {
	if (current === undefined) return { note: "not checked (file unavailable)" };
	const exact = findLines(current, before + selected + after, before.length);
	if (exact.length === 1) return { note: `selection and context found at line ${exact[0]}` };
	if (exact.length > 1) return { note: `selection and context found at lines ${exact.join(", ")}` };
	const loose = findLines(current, selected, 0);
	if (loose.length > 0) {
		const where = `line${loose.length === 1 ? "" : "s"} ${loose.join(", ")}`;
		return {
			note: `selected text found at ${where}, but not with its context`,
			warning: `the selection and its context were not found together in the saved file read as UTF-8; the selected text alone appears at ${where}`,
		};
	}
	return {
		note: "selected text not found",
		warning: "the selected text was not found in the saved file read as UTF-8",
	};
}

function findLines(haystack: string, needle: string, offset: number): number[] {
	const lines: number[] = [];
	if (!needle) return lines;
	for (let at = haystack.indexOf(needle); at !== -1 && lines.length < MAX_MATCHES; at = haystack.indexOf(needle, at + 1)) {
		let line = 1;
		for (let nl = haystack.indexOf("\n"); nl !== -1 && nl < at + offset; nl = haystack.indexOf("\n", nl + 1)) line++;
		lines.push(line);
	}
	return lines;
}

function describe(baseline: Baseline): string {
	if (baseline.kind === "git-index") return `Git index blob ${baseline.blobOid}`;
	const blob = baseline.blobOid ? `blob ${baseline.blobOid}` : "file absent there, so all current text is added";
	return `Git commit ${baseline.commitOid} (${blob})`;
}

function span(comment: ReviewComment): string {
	return comment.startLine === comment.endLine
		? `line ${comment.startLine}`
		: `lines ${comment.startLine}-${comment.endLine}`;
}

function q(path: string): string {
	return JSON.stringify(path);
}

function quote(label: string, body: string): string[] {
	let longest = 0;
	for (const run of body.match(/`+/g) ?? []) longest = Math.max(longest, run.length);
	const fence = "`".repeat(Math.max(3, longest + 1));
	return [`${label}:`, fence, body.endsWith("\n") ? body.slice(0, -1) : body, fence];
}

/** The inclusive source-line span of a unified hunk on one side; an empty side is an anchor. */
function hunkLines(hunk: string, side: ReviewComment["side"]): [number, number] | undefined {
	const header = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/.exec(hunk);
	if (!header) return undefined;
	const offset = side === "old" ? 1 : 3;
	const start = Number(header[offset]);
	const count = header[offset + 1] === undefined ? 1 : Number(header[offset + 1]);
	return count === 0 ? [start + 1, start + 1] : [start, start + count - 1];
}

function closestHunks(hunks: string[], comment: ReviewComment): string[] {
	const distances = hunks.map((hunk) => {
		const lines = hunkLines(hunk, comment.side);
		return lines ? Math.max(lines[0] - comment.endLine, comment.startLine - lines[1], 0) : Infinity;
	});
	const closest = distances.reduce((a, b) => Math.min(a, b), Infinity);
	return hunks.filter((_, i) => Number.isFinite(distances[i]) && distances[i] === closest);
}

const MAX_HUNK_LINES = 200;

/** Keeps whole hunks up to MAX_HUNK_LINES lines and replaces the rest with one summary entry. */
function capHunks(hunks: string[]): { hunks: string[]; truncated: boolean } {
	const lines = (hunk: string) => hunk.split("\n").length - (hunk.endsWith("\n") ? 1 : 0);
	const kept: string[] = [];
	let total = 0;
	for (const hunk of hunks) {
		if (total + lines(hunk) > MAX_HUNK_LINES) break;
		total += lines(hunk);
		kept.push(hunk);
	}
	const rest = hunks.slice(kept.length);
	if (rest.length === 0) return { hunks: kept, truncated: false };
	const omitted = rest.reduce((n, hunk) => n + lines(hunk), 0);
	const more = `${rest.length} more hunk${rest.length === 1 ? "" : "s"}`;
	return { hunks: [...kept, `... ${more} (${omitted} lines) omitted; read the file`], truncated: true };
}

export type WireBaseline =
	| { kind: "git-index"; blob_oid: string }
	| { kind: "git-commit"; commit_oid: string; blob_oid: string | null };

export interface HandoffComment {
	baseline?: WireBaseline;
	side: ReviewComment["side"];
	start_line: number;
	end_line: number;
	current_line: number;
	selected_text: string;
	context_before: string;
	context_after: string;
	enclosing_text?: string;
	context_hunks?: { hunks: string[]; truncated: boolean };
	comment: string;
}

export interface HandoffFile {
	path: string;
	user_edits: { against: "turn-snapshot" | "baseline" | "head"; hunks: string[]; truncated: boolean };
	review_comments: HandoffComment[];
}

export interface HandoffDetails {
	format: "helix-review-handoff";
	version: 1;
	baseline: string | null;
	files: HandoffFile[];
	request?: ReviewRequest;
	commit_patch?: CommitPatch;
}

export interface HandoffOptions {
	repo: string;
	/** Saved raw Git blobs from the end of the recipient's last turn, keyed by absolute path. */
	snapshots?: Record<string, string>;
	baseline?: string | null;
	excludePath?: (path: string) => boolean;
	/** Refuse unavailable existing content instead of treating it as empty; ENOENT still means deletion. */
	strictCurrentReads?: boolean;
	/** Pi's original handoff schema omits comment baselines; new consumers should retain them. */
	includeCommentBaselines?: boolean;
	commitPatch?: CommitPatch;
}

function wireBaseline(baseline: Baseline): WireBaseline {
	return baseline.kind === "git-index"
		? { kind: baseline.kind, blob_oid: baseline.blobOid }
		: { kind: baseline.kind, commit_oid: baseline.commitOid, blob_oid: baseline.blobOid };
}

/**
 * Build a transport-independent handoff using raw Git blobs and saved files. Ask requests
 * contain only commented files and nearby hunks, with bounded diff context. Pull request reviews
 * diff against HEAD without context, so each hunk maps onto the pull request's lines. No hooks,
 * external diff drivers or text conversions are run. This may store content in Git's object
 * database, but never stages, commits, or changes a working file.
 */
export async function buildHandoff(
	batch: ReviewBatch,
	options: HandoffOptions,
): Promise<{ details: HandoffDetails; content: string; files: HandoffFile[] } | undefined> {
	const scoped = batch.request?.kind === "ask";
	const review = batch.request?.kind === "pr-review";
	const baselines = new Map<string, Baseline>();
	for (const { path, baseline } of scoped ? batch.comments : [...batch.files, ...batch.comments]) {
		if (!baselines.has(path) && !options.excludePath?.(path)) baselines.set(path, baseline);
	}
	const files: HandoffFile[] = [];
	for (const [file, baseline] of baselines) {
		const saved = await readCurrent(file);
		if (options.strictCurrentReads && saved.problem && !saved.missing) {
			throw new BatchError(`Cannot verify ${q(file)}: ${saved.problem}. No deletion has been inferred.`);
		}
		const current = saved.text ?? "";
		let against: HandoffFile["user_edits"]["against"];
		let base: string | null;
		if (review) {
			const head = await readGitEntry(file, { kind: "git-commit", commitOid: "HEAD", blobOid: null });
			if (head.problem) throw new BatchError(`${q(file)} ${head.problem}.`);
			against = "head";
			base = head.oid ?? null;
		} else {
			const snapshot = options.snapshots?.[file];
			against = snapshot ? "turn-snapshot" : "baseline";
			base = snapshot ?? baseline.blobOid;
		}
		const oldOid = base ?? (await git(options.repo, ["hash-object", "-w", "--stdin"], "")).trim();
		const old = await git(options.repo, ["cat-file", "blob", oldOid]);
		let hunks: string[] = [];
		if (old !== current) {
			const newOid = (await git(options.repo, ["hash-object", "-w", "--no-filters", "--stdin"], current)).trim();
			const diff = await git(options.repo, ["diff", "--no-color", "--no-ext-diff", "--no-textconv", `--unified=${review ? 0 : 3}`, "--inter-hunk-context=0", oldOid, newOid]);
			hunks = diff.split(/^(?=@@ )/m).slice(1);
		}
		const comments = batch.comments.filter((comment) => comment.path === file);
		const contexts = new Map<ReviewComment, ReturnType<typeof capHunks>>();
		if (scoped) {
			const currentOid = (await git(options.repo, ["hash-object", "-w", "--no-filters", "--stdin"], current)).trim();
			const diffs = new Map<string, string[]>();
			const regions: [number, number][] = [];
			for (const comment of comments) {
				let line = comment.currentLine;
				if (comment.side === "current") {
					let found = findLines(current, comment.contextBefore + comment.selectedText + comment.contextAfter, comment.contextBefore.length);
					if (found.length !== 1) found = findLines(current, comment.selectedText, 0);
					if (found.length === 1) line = found[0];
				}
				const baseOid = comment.baseline.blobOid ?? (await git(options.repo, ["hash-object", "-w", "--stdin"], "")).trim();
				let diff = diffs.get(baseOid);
				if (!diff) {
					const text = await git(options.repo, ["diff", "--no-color", "--no-ext-diff", "--no-textconv", "--unified=3", "--inter-hunk-context=0", baseOid, currentOid]);
					diff = text.split(/^(?=@@ )/m).slice(1);
					diffs.set(baseOid, diff);
				}
				const nearby = closestHunks(diff, comment.side === "old" ? comment : { ...comment, startLine: line, endLine: line + comment.endLine - comment.startLine });
				contexts.set(comment, capHunks(nearby));
				for (const hunk of nearby) regions.push(hunkLines(hunk, "current")!);
				regions.push([Math.max(1, line - 3), line + comment.endLine - comment.startLine + 3]);
			}
			hunks = hunks.filter((hunk) => {
				const lines = hunkLines(hunk, "current");
				return lines && regions.some(([start, end]) => start <= lines[1] && lines[0] <= end);
			});
		}
		const reviewComments = comments.map((c): HandoffComment => ({
			...(options.includeCommentBaselines === false ? {} : { baseline: wireBaseline(c.baseline) }),
			side: c.side,
			start_line: c.startLine,
			end_line: c.endLine,
			current_line: c.currentLine,
			selected_text: c.selectedText,
			context_before: c.contextBefore,
			context_after: c.contextAfter,
			...(scoped ? { context_hunks: contexts.get(c) } : c.enclosingText === undefined ? {} : { enclosing_text: c.enclosingText }),
			comment: c.comment,
		}));
		if (hunks.length || reviewComments.length) {
			files.push({ path: file, user_edits: { against, ...capHunks(hunks) }, review_comments: reviewComments });
		}
	}
	const request = batch.request;
	if (files.length === 0 && !request) return undefined;
	const own = options.commitPatch;
	const details: HandoffDetails = {
		format: "helix-review-handoff",
		version: 1,
		baseline: options.baseline ?? null,
		files,
		...(request ? { request } : {}),
		...(own ? { commit_patch: own } : {}),
	};
	const content = [
		`The user reviewed ${review ? "a pull request" : "your changes"} in Helix and handed back ${files.length} file(s).`,
		scoped ? "Act on these comments using the selections and nearby hunks; unrelated changes are outside this request." : "Re-read those files from disk and act on the handoff below.",
		...quote("Handoff", JSON.stringify(details, null, 2)),
		review
			? "Leave the user's edits in the working tree: do not commit, stage, push or revert them."
			: "Integrate the user's edits and never revert them. Then continue editing or committing normally; that refreshes the review in Helix.",
		...(own && request?.kind === "commit-staged" ? patchSection(own, request.subject) : request ? requestSection(request, batch.comments) : []),
	].join("\n");
	return { details, content, files };
}

function git(cwd: string, args: string[], input?: string): Promise<string> {
	return new Promise((done, fail) => {
		const child = execFile(
			"git",
			["-C", cwd, ...args],
			{ encoding: "utf8", maxBuffer: 64 * 1024 * 1024, windowsHide: true },
			(error, stdout) => (error ? fail(error) : done(stdout)),
		);
		if (input !== undefined) child.stdin?.end(input);
	});
}
