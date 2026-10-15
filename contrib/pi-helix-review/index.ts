import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { constants, existsSync, type FSWatcher, watch } from "node:fs";
import { access, mkdir, readdir, readFile, realpath, rename, stat, unlink, writeFile } from "node:fs/promises";
import { connect } from "node:net";
import { homedir } from "node:os";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { promisify } from "node:util";
import type { ExtensionAPI, ExtensionCommandContext } from "@earendil-works/pi-coding-agent";

const MAX_BATCH_BYTES = 4 * 1024 * 1024;
const MAX_ENTRIES = 1000;
const MAX_SOURCE_BYTES = 8 * 1024 * 1024;
const MAX_MATCHES = 20;

const run = promisify(execFile);

class BatchError extends Error {}

type Baseline =
	| { kind: "git-index"; blobOid: string }
	| { kind: "git-commit"; commitOid: string; blobOid: string | null };

interface ReviewFile {
	path: string;
	baseline: Baseline;
}

interface ReviewComment {
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

interface GitEntry {
	oid?: string | null;
	problem?: string;
}

interface FileState {
	text?: string;
	problem?: string;
	// Raw entries keyed by "index" or by commit id, read only for baselines the batch uses.
	git: Map<string, GitEntry>;
}

/** What the reviewer asks the agent to do with a handoff, beyond the edits and comments. */
type ReviewRequest = { kind: "commit-staged"; subject?: string } | { kind: "message"; text: string } | { kind: "ask" };

class ReviewBatch {
	files: ReviewFile[];
	comments: ReviewComment[];
	request?: ReviewRequest;
	/** The session id or label of the pi session the batch is for, if Helix named one. */
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
	throw new BatchError(`request.kind must be "commit-staged", "message" or "ask"`);
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
					...(comment.enclosingText === undefined ? [] : quote("Enclosing code", comment.enclosingText)),
					...quote("Message", comment.comment),
				];
			}),
		];
	}
	const subject = request.subject ? ` Use this subject: ${JSON.stringify(request.subject)}.` : "";
	return [
		"User request:",
		`Commit exactly what is staged in the git index: run \`git commit\` without \`-a\` and without staging anything first, with a Conventional Commit message derived from \`git diff --cached\`.${subject} Do not stage, unstage or modify anything else, leave unstaged changes alone, and report the commit hash.`,
	];
}

/** A staged hunk: 0-based `HEAD` and index line spans of `name` and the blobs they refer to. */
interface StagedHunk {
	name: string;
	head: string;
	index: string;
	os: number;
	oc: number;
	ns: number;
	nc: number;
}

/** The patch with only this session's staged hunks, and those hunks as `path:lines`. */
interface CommitPatch {
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

/** The agent owning line `line` of `path` in the ledger: the range's owner, or the agent a user edit overwrote. */
function ownerAt(ownership: Ownership, path: string, line: number): string | undefined {
	const ranges = ownership.files[path]?.ranges ?? [];
	const range = ranges.find((r) => r.start <= line && line < r.end) ?? ranges.find((r) => r.start === r.end && r.start === line);
	return range && (range.owner === "user" ? range.agent : range.owner);
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

async function readCurrent(path: string): Promise<Omit<FileState, "git">> {
	try {
		const info = await stat(path);
		if (!info.isFile()) return { problem: "is not a regular file" };
		if (info.size > MAX_SOURCE_BYTES) return { problem: "is larger than 8 MiB, so its saved content was not checked" };
		return { text: (await readFile(path)).toString("utf8") };
	} catch (error) {
		if ((error as { code?: string }).code === "ENOENT") return { problem: "is missing on disk" };
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

/** One pi session's hook state, in `state/<sessionId>.json`. */
interface HookState {
	/** `HEAD` as last seen: the review compares against it. (`acknowledged` is from earlier versions.) */
	baseline?: { commit_oid: string; acknowledged?: boolean };
	snapshots: Record<string, string>;
	/** When each file was last changed during one of this session's turns (ms since the epoch). */
	edited?: Record<string, number>;
	/**
	 * Set by a commit-staged handoff, to `HEAD` at the time, until the end of the turn it starts
	 * or steers.
	 */
	commit_requested?: string | boolean;
	/**
	 * The commit a commit-staged handoff made: every session's baseline older than it moves up to
	 * it, so the reviewer stops seeing hunks they asked to commit.
	 */
	commit_floor?: string;
	/** Updated at session start and at the start and end of each turn (ms since the epoch). */
	last_active?: number;
	/** The session's name in Helix's status line, `pi-<N>`, unique among live sessions. */
	label?: string;
	cwd?: string;
	/** The pi process, so other sessions can tell whether this one is live; cleared on shutdown. */
	pid?: number;
	/** Whether an agent turn of this session is running. */
	in_turn?: boolean;
	/** When the running turn started (ms since the epoch); cleared at its end. */
	turn_started?: number;
	/** Set while this session commits its share of a staged set that holds other sessions' hunks. */
	commit_chain?: boolean;
}

interface Session {
	id: string;
	state: HookState;
}

/** Files larger than this get no line ownership. */
const MAX_OWNED_BYTES = 1024 * 1024;

/** How long a claimed batch of a session that is gone waits before it is put back in the outbox. */
const STALE_CLAIM_MS = 10 * 60 * 1000;

/**
 * Lines of a file one writer changed last: a session label, or `user`. `start === end` marks lines
 * deleted just before line `start`. `agent` is the last agent owner, kept when the user overwrites.
 */
export interface OwnedRange {
	start: number;
	end: number;
	owner: string;
	agent?: string;
	at: number;
}

/** `state/ownership.json`: per file, the blob its ranges refer to and the uncommitted ranges. */
export interface Ownership {
	files: Record<string, { blob: string; ranges: OwnedRange[] }>;
}

type Mark = { owner: string; agent?: string; at: number } | undefined;

/** Per-line marks, and per-gap deletion marks (gap `i` is just before line `i + 1`). */
interface Marks {
	lines: Mark[];
	gaps: Mark[];
}

function lineCount(text: string): number {
	return text === "" ? 0 : text.split("\n").length - (text.endsWith("\n") ? 1 : 0);
}

/** The `git diff -U0` hunks from blob `from` to blob `to` as 0-based [old start, old count, new start, new count]. */
async function blobHunks(repo: string, from: string, to: string): Promise<[number, number, number, number][]> {
	if (from === to) return [];
	const diff = await git(repo, ["diff", "-U0", "--no-color", "--no-ext-diff", "--no-textconv", from, to]);
	return [...diff.matchAll(/^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/gm)].map(([, a, b, c, d]) => {
		const [oc, nc] = [b === undefined ? 1 : Number(b), d === undefined ? 1 : Number(d)];
		// A count of 0 names the line after which the change is; otherwise its first line.
		return [oc > 0 ? Number(a) - 1 : Number(a), oc, nc > 0 ? Number(c) - 1 : Number(c), nc];
	});
}

function toMarks(ranges: OwnedRange[], lines: number): Marks {
	const marks: Marks = { lines: Array(lines).fill(undefined), gaps: Array(lines + 1).fill(undefined) };
	for (const { start, end, owner, agent, at } of ranges) {
		const mark = { owner, agent, at };
		if (start === end) {
			if (start - 1 <= lines) marks.gaps[start - 1] = mark;
		} else {
			for (let i = start - 1; i < Math.min(end - 1, lines); i++) marks.lines[i] = mark;
		}
	}
	return marks;
}

function toRanges({ lines, gaps }: Marks): OwnedRange[] {
	const ranges: OwnedRange[] = [];
	const range = (start: number, end: number, m: NonNullable<Mark>): OwnedRange => ({
		start,
		end,
		owner: m.owner,
		...(m.agent ? { agent: m.agent } : {}),
		at: m.at,
	});
	for (let i = 0; i <= lines.length; i++) {
		const gap = gaps[i];
		if (gap) ranges.push(range(i + 1, i + 1, gap));
		const mark = lines[i];
		if (!mark) continue;
		const last = ranges.at(-1);
		if (last && last.end === i + 1 && last.start < last.end && last.owner === mark.owner && last.agent === mark.agent && last.at === mark.at) {
			last.end++;
		} else {
			ranges.push(range(i + 1, i + 2, mark));
		}
	}
	return ranges;
}

/**
 * Carries `marks` through the change `hunks` describes: lines outside them keep their marks
 * (shifted), the new lines of each hunk (or, for a pure deletion, its gap) get `mark(replaced)`.
 */
function applyHunks(marks: Marks, hunks: [number, number, number, number][], mark: (replaced: Mark[]) => Mark): Marks {
	const lines: Mark[] = [];
	const gaps: Mark[] = [];
	let oi = 0;
	const copyUntil = (end: number) => {
		for (; oi < end; oi++) {
			gaps[lines.length] ??= marks.gaps[oi];
			lines.push(marks.lines[oi]);
		}
	};
	for (const [os, oc, , nc] of hunks) {
		copyUntil(os);
		const written = mark([...marks.lines.slice(os, os + oc), ...marks.gaps.slice(os, os + oc + 1)]);
		if (nc === 0) gaps[lines.length] = written;
		for (let k = 0; k < nc; k++) lines.push(written);
		oi = os + oc;
	}
	copyUntil(marks.lines.length);
	gaps[lines.length] ??= marks.gaps[oi];
	return { lines, gaps: Array.from({ length: lines.length + 1 }, (_, i) => gaps[i]) };
}

async function readOwnership(path: string): Promise<Ownership> {
	try {
		const parsed = JSON.parse(await readFile(path, "utf8")) as Ownership;
		return parsed && typeof parsed.files === "object" ? parsed : { files: {} };
	} catch {
		return { files: {} };
	}
}

async function writeOwnership(path: string, ownership: Ownership): Promise<void> {
	const temp = `${path}.${process.pid}.tmp`;
	await writeFile(temp, `${JSON.stringify(ownership, null, 2)}\n`);
	await rename(temp, path);
}

const GUIDELINES = `## Helix review handoff
The user reviews your changes in Helix (in this repository or any of its worktrees you edit) and hands back explicit requests as a "helix-review:handoff" message with a JSON block, possibly in the middle of a turn as a steering message: per file, review_comments are the user's comments on selections, user_edits are the user's own edits since your turn snapshot as unified-diff hunks (integrate them, never revert them; when they undo part of your change, keep it undone and adjust tests and docs to match), and a request may carry a message or ask for a commit. The review always compares against HEAD: commits of any kind move it, except a baseline the user pinned in Helix. After handling a handoff, say per file what you integrated or declined. While this hook is attached, never run git commit or git push on your own, even if other instructions say to commit at the end of a turn: the user stages hunks in Helix and asks for a commit with a commit-staged handoff, the only time you commit (exactly the index, adding nothing).`;

class Hook {
	private pi: ExtensionAPI;
	readonly repo: string;
	readonly dir: string;
	/** The pi session id, made safe for file names. */
	readonly id: string;
	private state: HookState;
	/** The changed files and their blobs at the start of the running turn. */
	private turnStart?: Record<string, string>;
	/** Updates of the ownership ledger, one at a time. */
	private ledger = Promise.resolve();
	/** Paths whose failed attribution was already logged. */
	private warned = new Set<string>();
	/** What the last check for user edits saw: `git status` and the changed files' stats. */
	private lastStatus?: string;
	private watcher?: FSWatcher;
	private timer?: NodeJS.Timeout;
	private poller?: NodeJS.Timeout;
	private attaching = false;
	/** Remapped baselines by `<baseline>@<HEAD>`. */
	private remapped = new Map<string, string>();
	/** The base branch, `null` when there is none; unset until looked up. */
	private base?: string | null;
	/**
	 * The identity (inode and change time, as an inode number can be reused) of the Helix socket
	 * last attached to; unset while no Helix is known to listen.
	 */
	private inode?: string;
	private stopped = false;
	/** The running (or last) poller tick, so stopping can wait for it. */
	private polling = Promise.resolve();
	private stopping?: Promise<void>;
	private queue = Promise.resolve();
	private seen = new Set<string>();

	constructor(pi: ExtensionAPI, repo: string, dir: string, id: string, state: HookState) {
		this.pi = pi;
		this.repo = repo;
		this.dir = dir;
		this.id = id;
		this.state = state;
	}

	static async open(pi: ExtensionAPI, cwd: string, sessionId: string): Promise<Hook | undefined> {
		let repo: string;
		try {
			repo = await realpath((await git(cwd, ["rev-parse", "--show-toplevel"])).trim());
		} catch {
			return undefined;
		}
		const dir = join(repo, ".pi", "helix-review");
		await mkdir(join(dir, "outbox"), { recursive: true });
		await mkdir(join(dir, "state"), { recursive: true });
		if (!existsSync(join(dir, ".gitignore"))) await writeFile(join(dir, ".gitignore"), "*\n");
		const id = sessionId.replace(/[^\w-]/g, "_");
		let state = await readState(join(dir, "state", `${id}.json`));
		if (!state) {
			// The single state.json of earlier versions moves into the first session that starts.
			state = await readState(join(dir, "state.json"));
			if (state) await unlink(join(dir, "state.json")).catch(() => undefined);
		}
		const hook = new Hook(pi, repo, dir, id, { ...(state ?? { snapshots: {} }), cwd });
		await hook.removeDeadSessions();
		const taken = new Set((await hook.liveSessions()).filter((s) => s.id !== id).map((s) => s.state.label));
		if (!hook.state.label || taken.has(hook.state.label)) {
			let n = 1;
			while (taken.has(`pi-${n}`)) n++;
			hook.state.label = `pi-${n}`;
		}
		await hook.touch();
		return hook;
	}

	get label(): string {
		return this.state.label!;
	}

	/**
	 * The baseline Helix reviews against: the newest commit floor of the live sessions (each
	 * session's `HEAD` as it last saw it), else this session's baseline.
	 */
	private async reviewBaseline(): Promise<string | undefined> {
		return (await this.commitFloor(await this.liveSessions())) ?? this.state.baseline?.commit_oid;
	}

	/**
	 * Moves this session's baseline (and floor) to `HEAD` when a commit of any kind moved it, or
	 * sets it when there is none yet and `start`. Returns whether it changed.
	 */
	private async followHead(start = false): Promise<boolean> {
		if (!this.state.baseline && !start) return false;
		const head = await git(this.repo, ["rev-parse", "HEAD"]).then((out) => out.trim(), () => "");
		if (!head || head === this.state.baseline?.commit_oid) return false;
		this.state.baseline = { commit_oid: head };
		this.state.commit_floor = head;
		await this.save();
		await this.pruneOwnership();
		return true;
	}

	/** The newest commit floor of `sessions` that is still in `HEAD`'s history, remapped after rebases. */
	private async commitFloor(sessions: Session[]): Promise<string | undefined> {
		const floors: string[] = [];
		for (const floor of new Set(sessions.map((s) => s.state.commit_floor).filter((f): f is string => !!f))) {
			const remapped = await this.remap(floor);
			if (await this.isAncestor(remapped, "HEAD")) floors.push(remapped);
		}
		return (await this.newestFirst([...new Set(floors)]))[0];
	}

	/** `oids` from the newest commit to the oldest. */
	private async newestFirst(oids: string[]): Promise<string[]> {
		if (oids.length <= 1) return oids;
		const out = await git(this.repo, ["log", "--no-walk=sorted", "--format=%H", ...oids]).catch(() => "");
		return out.trim().split("\n").filter(Boolean);
	}

	private isAncestor(oid: string, of: string): Promise<boolean> {
		return git(this.repo, ["merge-base", "--is-ancestor", oid, of]).then(() => true, () => false);
	}

	/**
	 * The commit baseline `oid` stands for in the current history: after the base branch moved
	 * under a branch that started at `oid`, the new merge base; after a rebase rewrote `oid`, the
	 * commit with the same patch-id on the branch, or the merge base when it was dropped or
	 * squashed; else `oid` itself.
	 */
	private async remap(oid: string): Promise<string> {
		const head = await git(this.repo, ["rev-parse", "HEAD"]).then((out) => out.trim(), () => "");
		const key = `${oid}@${head}`;
		let remapped = this.remapped.get(key);
		if (remapped === undefined) {
			remapped = (await this.findRewritten(oid, head).catch(() => undefined)) ?? oid;
			this.remapped.set(key, remapped);
			if (remapped !== oid) {
				console.info(`helix-review: baseline ${oid.slice(0, 8)} rewritten as ${remapped.slice(0, 8)} by a rebase`);
			}
		}
		return remapped;
	}

	private async findRewritten(oid: string, head: string): Promise<string | undefined> {
		if (!head || oid === head) return undefined;
		const succeeds = (args: string[]) => git(this.repo, args).then(() => true, () => false);
		const base = await this.baseBranch();
		const mergeBase = base
			? await git(this.repo, ["merge-base", base, "HEAD"]).then((out) => out.trim(), () => undefined)
			: undefined;
		if (await succeeds(["merge-base", "--is-ancestor", oid, "HEAD"])) {
			// The branch's old base: follow the base branch when it moved on under the branch.
			if (!base || !mergeBase || mergeBase === oid || mergeBase === head) return undefined;
			const onBase = await succeeds(["merge-base", "--is-ancestor", oid, base]);
			const moved = await succeeds(["merge-base", "--is-ancestor", oid, mergeBase]);
			return onBase && moved ? mergeBase : undefined;
		}
		// Rewritten by a rebase: the branch commit with the same change.
		const patchId = async (patches: string) =>
			(await git(this.repo, ["patch-id", "--stable"], patches)).split("\n").filter(Boolean).map((line) => line.split(" "));
		const own = await git(this.repo, ["show", oid])
			.then(patchId)
			.catch(() => []);
		if (own.length > 0) {
			const range = base ? `${base}..HEAD` : "HEAD";
			const log = await git(this.repo, ["log", "-p", "--first-parent", "--max-count=500", range]);
			const match = (await patchId(log)).find(([id]) => id === own[0][0]);
			if (match) return match[1];
		}
		return mergeBase;
	}

	/**
	 * The branch the current branch is based on: its pull request's base (looked up once with
	 * `gh`), else `origin/HEAD`, else the first of the usual names that exists. Cached.
	 */
	private async baseBranch(): Promise<string | undefined> {
		if (this.base !== undefined) return this.base ?? undefined;
		const exists = (ref: string) =>
			git(this.repo, ["rev-parse", "--verify", "--quiet", `${ref}^{commit}`]).then(() => true, () => false);
		const gh = process.env.HELIX_GH || "gh";
		const pr = await run(gh, ["pr", "view", "--json", "baseRefName"], { cwd: this.repo, timeout: 10_000 })
			.then(({ stdout }) => (JSON.parse(stdout) as { baseRefName?: string }).baseRefName)
			.catch(() => undefined);
		const remoteHead = await git(this.repo, ["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])
			.then((out) => out.trim().replace(/^refs\/remotes\//, ""))
			.catch(() => undefined);
		const candidates = [
			...(pr ? [`origin/${pr}`, pr] : []),
			...(remoteHead ? [remoteHead] : []),
			...["origin/develop", "origin/main", "origin/master", "develop", "main", "master"],
		];
		this.base = null;
		for (const ref of candidates) {
			if (await exists(ref)) {
				this.base = ref;
				break;
			}
		}
		return this.base ?? undefined;
	}

	/** Marks this session live and active now. */
	async touch(): Promise<void> {
		this.state.pid = process.pid;
		this.state.last_active = Date.now();
		await this.save();
	}

	/** The sessions in this repository whose pi process is running, this one included. */
	private async liveSessions(): Promise<Session[]> {
		const sessions = [{ id: this.id, state: this.state }];
		for (const name of await readdir(join(this.dir, "state")).catch(() => [] as string[])) {
			const id = name.endsWith(".json") ? name.slice(0, -5) : undefined;
			if (!id || id === this.id) continue;
			const state = await readState(join(this.dir, "state", name));
			if (state && alive(state.pid)) sessions.push({ id, state });
		}
		return sessions;
	}

	/** Deletes the state files of sessions whose pi process is gone. */
	private async removeDeadSessions(): Promise<void> {
		for (const name of await readdir(join(this.dir, "state")).catch(() => [] as string[])) {
			if (!name.endsWith(".json") || name === "ownership.json" || name === `${this.id}.json`) continue;
			const path = join(this.dir, "state", name);
			const state = await readState(path);
			if (state && !alive(state.pid)) await unlink(path).catch(() => undefined);
		}
	}

	/**
	 * Of `sessions`, the least busy: the longest idle of those not in a turn, else the one whose
	 * turn started first.
	 */
	private idlest(sessions: Session[]): Session {
		const idle = sessions.filter((s) => !s.state.in_turn);
		const pool = idle.length > 0 ? idle : sessions;
		const key = (s: Session) => (idle.length > 0 ? s.state.last_active : s.state.turn_started) ?? 0;
		return pool.reduce((a, b) => (key(b) < key(a) || (key(b) === key(a) && b.id < a.id) ? b : a));
	}

	/**
	 * The session `batch` is for: the one it targets, else the live session that last changed one
	 * of its files during a turn, else the least busy live session.
	 */
	private async addressee(batch: ReviewBatch): Promise<string | undefined> {
		const sessions = await this.liveSessions();
		if (batch.target !== undefined) {
			return sessions.find((s) => s.id === batch.target || s.state.label === batch.target)?.id ?? batch.target;
		}
		const paths = [...batch.files, ...batch.comments].map((entry) => entry.path);
		const newest = (key: (s: Session) => number) =>
			sessions.reduce((a, b) => (key(b) > key(a) || (key(b) === key(a) && b.id < a.id) ? b : a));
		const edited = (s: Session) => Math.max(0, ...paths.map((path) => s.state.edited?.[path] ?? 0));
		const editor = newest(edited);
		return edited(editor) > 0 ? editor.id : this.idlest(sessions).id;
	}

	get outbox(): string {
		return join(this.dir, "outbox");
	}

	get socket(): string {
		if (process.env.HELIX_COMMAND_SOCKET) return process.env.HELIX_COMMAND_SOCKET;
		const id = createHash("sha256").update(this.repo).digest("hex").slice(0, 32);
		const runtime = process.env.XDG_RUNTIME_DIR;
		return join(runtime ? join(runtime, "helix") : `/tmp/helix-${process.getuid?.()}`, `${id}.sock`);
	}

	async start(): Promise<void> {
		await this.sweep();
		this.watcher = watch(this.outbox, (_event, name) => name && this.enqueue(name));
		void readdir(this.outbox).then((names) => names.forEach((name) => this.enqueue(name)));
		// Watches the socket: a new Helix (a new socket file) is attached to as soon as it appears.
		this.poller = setInterval(() => {
			// One tick at a time; a slow one delays the next instead of overlapping it.
			this.polling = this.polling.then(() => this.poll()).catch(() => undefined);
		}, 2000);
		this.poller.unref();
	}

	/**
	 * Puts batches claimed long ago by sessions that are gone back in the outbox, and deletes the
	 * state files of those sessions.
	 */
	private async sweep(): Promise<void> {
		const live = new Set((await this.liveSessions()).map((s) => s.id));
		await this.removeDeadSessions();
		for (const name of await readdir(this.outbox)) {
			const claim = /^(.+)\.([^.]+)\.claimed$/.exec(name);
			if (!claim || live.has(claim[2])) continue;
			const path = join(this.outbox, name);
			const info = await stat(path).catch(() => undefined);
			if (info && Date.now() - info.mtimeMs > STALE_CLAIM_MS) {
				await rename(path, join(this.outbox, `${claim[1]}.json`)).catch(() => undefined);
			}
		}
	}

	/** Stops watching and polling, and resolves once the running poller tick and ledger updates are done. */
	stop(): Promise<void> {
		this.stopping ??= (async () => {
			this.stopped = true;
			this.watcher?.close();
			clearTimeout(this.timer);
			clearInterval(this.poller);
			this.state.pid = undefined;
			this.state.in_turn = undefined;
			this.state.turn_started = undefined;
			await this.polling;
			await this.ledger;
			await this.save().catch(() => undefined);
		})();
		return this.stopping;
	}

	/** Starts a turn; `midTurn` for a hook opened during it, whose changes all count as the turn's. */
	async begin(midTurn = false): Promise<void> {
		await this.followHead(true);
		this.turnStart = midTurn ? {} : await this.changedFiles();
		this.state.in_turn = true;
		this.state.turn_started = Date.now();
		await this.touch();
		await this.attach();
	}

	/** Points Helix at the outbox and restores the baseline. Returns whether Helix was reachable. */
	async attach(): Promise<boolean> {
		const inode = socketId(await stat(this.socket).catch(() => undefined));
		const commands = [`review-handoff ${this.outbox}`];
		await this.followHead();
		const baseline = await this.reviewBaseline();
		if (baseline) commands.push(`review-baseline ${baseline}`);
		const reachable = await this.send(commands);
		if (reachable) this.inode = inode;
		return reachable;
	}

	/** Sends `commands` to Helix; when no Helix listens, the poller attaches to the next one. */
	private async send(commands: string[]): Promise<boolean> {
		const reachable = await helix(this.socket, commands);
		if (!reachable) this.inode = undefined;
		return reachable;
	}

	/**
	 * Attaches again when the socket is a new file (Helix restarted, or started after pi), and
	 * moves the review on when a commit made anywhere moved `HEAD`.
	 */
	private async poll(): Promise<void> {
		if (this.attaching || this.stopped) return;
		const moved = await this.followHead();
		await this.attributeUserEdits();
		const info = await stat(this.socket).catch(() => undefined);
		if (!info) {
			this.inode = undefined;
			return;
		}
		if (socketId(info) === this.inode) {
			if (moved) await this.send([`review-baseline ${await this.reviewBaseline()}`]);
			return;
		}
		this.attaching = true;
		try {
			if (await this.attach()) await this.announceAttach();
		} finally {
			this.attaching = false;
		}
	}

	refreshSoon(): void {
		clearTimeout(this.timer);
		this.timer = setTimeout(() => void this.refresh(), 200);
	}

	async refresh(): Promise<void> {
		clearTimeout(this.timer);
		await this.followHead();
		await this.attributeUserEdits();
		const baseline = await this.reviewBaseline();
		if (baseline) await this.send([`review-baseline ${baseline}`]);
	}

	/** Shows `message` in Helix's status line with `:echo`; a failure is ignored like any other. */
	async echo(message: string): Promise<void> {
		await this.send([`echo ${message}`]);
	}

	/** Tells the reviewer in Helix that the hook is linked, and with which restored baseline. */
	async announceAttach(): Promise<void> {
		const commit = await this.reviewBaseline();
		const baseline = commit ? `, baseline ${commit.slice(0, 8)}` : "";
		await this.echo(`pi hook attached as ${this.label}${baseline}`);
	}

	/**
	 * At the end of a turn that was asked to commit the staged hunks, moves the baseline to the new
	 * `HEAD` and makes Helix adopt it even over a pinned baseline. Returns the new commit.
	 */
	async followRequestedCommit(): Promise<string | undefined> {
		const requested = this.state.commit_requested;
		if (!requested) return undefined;
		this.state.commit_requested = false;
		await this.save();
		const chained = this.state.commit_chain;
		this.state.commit_chain = undefined;
		const before = typeof requested === "string" ? requested : this.state.baseline?.commit_oid;
		const head = await git(this.repo, ["rev-parse", "HEAD"]).then((out) => out.trim(), () => "");
		if (!head || head === before) return undefined;
		await this.followHead(true);
		if (chained) await this.passCommitOn();
		// Asking for a commit ends any baseline the reviewer pinned in Helix.
		await this.send([`review-baseline ${head} force`]);
		return head;
	}

	/**
	 * Tells the reviewer in Helix that the turn ended and how many files differ from the baseline;
	 * with `quiet`, only when some do.
	 */
	async announceEnd(committed?: string, quiet = false): Promise<void> {
		if (!this.state.baseline) return;
		const count = Object.keys(this.state.snapshots).length;
		if (quiet && count === 0 && !committed) return;
		if (committed) {
			const files = count === 1 ? "1 file" : `${count} files`;
			return this.echo(`${this.label}: Agent turn done, committed ${committed.slice(0, 8)}, ${files} still changed`);
		}
		const since = this.state.baseline.commit_oid.slice(0, 8);
		if (count === 0) return this.echo(`${this.label}: Agent turn done, no file changes since ${since}`);
		const files = count === 1 ? "1 file" : `${count} files`;
		await this.echo(`${this.label}: Agent turn done, ${files} changed since ${since} (:review-files)`);
	}

	/** Stores the turn's snapshots and which files the turn changed, and marks the session active. */
	async snapshot(): Promise<void> {
		if (!this.state.baseline) return;
		const now = await this.changedFiles();
		const at = Date.now();
		if (this.turnStart) {
			this.state.edited ??= {};
			for (const [path, oid] of Object.entries(now)) if (this.turnStart[path] !== oid) this.state.edited[path] = at;
		}
		this.turnStart = undefined;
		this.state.snapshots = now;
		this.state.last_active = at;
		await this.save();
	}

	/** The blobs (`git hash-object -w`) of every file changed since the baseline, untracked ones included. */
	private async changedFiles(): Promise<Record<string, string>> {
		const paths = await this.changedPaths();
		const oids = paths.length ? (await git(this.repo, ["hash-object", "-w", "--no-filters", "--", ...paths])).split("\n") : [];
		return Object.fromEntries(paths.map((path, i) => [path, oids[i]]));
	}

	/** The readable files changed since the baseline, untracked ones included. */
	private async changedPaths(): Promise<string[]> {
		if (!this.state.baseline) return [];
		const changed = await git(this.repo, ["diff", "--name-only", "-z", this.state.baseline.commit_oid]);
		const untracked = await git(this.repo, ["ls-files", "--others", "--exclude-standard", "-z"]);
		const paths: string[] = [];
		for (const name of new Set(`${changed}${untracked}`.split("\0").filter(Boolean))) {
			const path = join(this.repo, name);
			const readable = await access(path, constants.R_OK).then(() => true, () => false);
			if (!this.isPiState(path) && readable && (await stat(path).catch(() => undefined))?.isFile()) paths.push(path);
		}
		return paths;
	}

	/** The repository's shared ownership ledger. */
	get ownershipFile(): string {
		return join(this.dir, "state", "ownership.json");
	}

	/**
	 * Records `writer` (this session's label, or `user`) as the owner of what changed in `paths`
	 * since the ledger last saw them; without `paths`, of every file that differs from the ledger.
	 */
	attribute(paths: string[] | undefined, writer: string): Promise<void> {
		return this.withLedger(async (ownership) => {
			const candidates = paths ?? [...new Set([...(await this.changedPaths()), ...Object.keys(ownership.files)])];
			let changed = false;
			for (const path of candidates) {
				if (!this.owns(path)) continue;
				await this.guarded(path, async () => {
					let entry = ownership.files[path];
					if (!(await this.attributable(path))) {
						if (entry) delete ownership.files[path];
						changed ||= !!entry;
						return;
					}
					const blob = (await git(this.repo, ["hash-object", "-w", "--no-filters", "--", path])).trim();
					if (entry?.blob === blob) return;
					const head = await this.headBlob(path);
					if (!entry) {
						// What differed from HEAD before this change is the user's.
						entry = await this.changeEntry({ blob: head, ranges: [] }, this.turnStart?.[path] ?? head, "user");
					}
					entry = await this.pruneEntry(await this.changeEntry(entry, blob, writer), head);
					if (entry.ranges.length > 0) ownership.files[path] = entry;
					else delete ownership.files[path];
					changed = true;
				});
			}
			return changed;
		});
	}

	/** Whether `path` (resolved) is a file of this repository that ownership is kept for. */
	private owns(path: string): boolean {
		return path.startsWith(this.repo + sep) && !this.isPiState(path);
	}

	/** Whether `path` is a text file small enough to keep line ownership for. */
	private async attributable(path: string): Promise<boolean> {
		const info = await stat(path).catch(() => undefined);
		if (!info?.isFile() || info.size > MAX_OWNED_BYTES) return false;
		// Git's own test for binary content: a NUL byte in the first 8000 bytes.
		return !(await readFile(path)).subarray(0, 8000).includes(0);
	}

	/** Runs the ledger update `body` of `path`; a failure skips that path, logged once per session. */
	private async guarded(path: string, body: () => Promise<void>): Promise<void> {
		try {
			await body();
		} catch (error) {
			if (this.stopped || this.warned.has(path)) return;
			this.warned.add(path);
			console.warn(`helix-review: ownership of ${path} not updated: ${error}`);
		}
	}

	/**
	 * The ownership ledger, without entries for files outside the repository, gone, too large or
	 * binary; written back when it held any.
	 */
	private async readLedger(): Promise<Ownership> {
		const ownership = await readOwnership(this.ownershipFile);
		let dropped = false;
		for (const path of Object.keys(ownership.files)) {
			// One that cannot be read stays: its update fails, and is logged, instead.
			if (this.owns(path) && (await this.attributable(path).catch(() => true))) continue;
			delete ownership.files[path];
			dropped = true;
		}
		if (dropped) await writeOwnership(this.ownershipFile, ownership);
		return ownership;
	}

	/** Drops the ranges a commit took in: the ledger only holds uncommitted work. */
	private pruneOwnership(): Promise<void> {
		return this.withLedger(async (ownership) => {
			for (const [path, entry] of Object.entries(ownership.files)) {
				await this.guarded(path, async () => {
					const pruned = await this.pruneEntry(entry, await this.headBlob(path));
					if (pruned.ranges.length > 0) ownership.files[path] = pruned;
					else delete ownership.files[path];
				});
			}
			return existsSync(this.ownershipFile);
		});
	}

	/**
	 * Attributes changes nobody announced to the user: only while no session, this one included,
	 * is in a turn.
	 */
	private async attributeUserEdits(): Promise<void> {
		if (this.state.in_turn || !this.state.baseline) return;
		if ((await this.liveSessions()).some((s) => s.state.in_turn)) return;
		// Only when `git status` or a changed file's size or time moved since the last check.
		const status = await git(this.repo, ["status", "--porcelain", "-z", "--untracked-files=all"]);
		const listed = status.split("\0").filter(Boolean).map((entry) => join(this.repo, entry.slice(3)));
		const paths = [...new Set([...listed, ...Object.keys((await this.readLedger()).files)])];
		const stats = await Promise.all(paths.map((path) => stat(path).then((s) => `${path}:${s.size}:${s.mtimeMs}`, () => path)));
		const seen = [status, ...stats].join("\n");
		if (seen === this.lastStatus) return;
		this.lastStatus = seen;
		await this.attribute(undefined, "user");
	}

	/** Ends the turn: changes still unannounced are this session's. */
	async endTurn(): Promise<void> {
		await this.attribute(undefined, this.label);
		this.state.in_turn = false;
		this.state.turn_started = undefined;
		await this.save();
	}

	/** Runs `update` on the ledger, one update at a time, and writes it when `update` says it changed. */
	private withLedger(update: (ownership: Ownership) => Promise<boolean>): Promise<void> {
		// shortcut: serialized per session only; two sessions updating at the same instant can lose one update.
		const run = this.ledger.then(async () => {
			// A stopped hook leaves the ledger alone: its repository may be gone already.
			if (this.stopped) return;
			const ownership = await this.readLedger();
			if ((await update(ownership)) && !this.stopped) await writeOwnership(this.ownershipFile, ownership);
		});
		this.ledger = run.catch((error) => {
			if (!this.stopped) console.warn(`helix-review: ownership not updated: ${error}`);
		});
		return this.ledger;
	}

	/** The blob of `path` in `HEAD`, or the empty blob. */
	private async headBlob(path: string): Promise<string> {
		const inHead = await git(this.repo, ["rev-parse", "--verify", "--quiet", `HEAD:${relative(this.repo, path)}`]).catch(() => "");
		return inHead.trim() || (await git(this.repo, ["hash-object", "-w", "--stdin"], "")).trim();
	}

	private async lines(blob: string): Promise<number> {
		return lineCount(await git(this.repo, ["cat-file", "blob", blob]));
	}

	/** `entry` carried to the content `blob`, the changed lines owned by `writer`. */
	private async changeEntry(entry: Ownership["files"][string], blob: string, writer: string): Promise<Ownership["files"][string]> {
		if (entry.blob === blob) return entry;
		const at = Date.now();
		const marks = applyHunks(toMarks(entry.ranges, await this.lines(entry.blob)), await blobHunks(this.repo, entry.blob, blob), (replaced) => {
			if (writer !== "user") return { owner: writer, agent: writer, at };
			const agent = replaced.filter((m) => m?.agent).sort((x, y) => y!.at - x!.at)[0]?.agent;
			return { owner: "user", agent, at };
		});
		return { blob, ranges: toRanges(marks) };
	}

	/** `entry` without the marks on lines (and deletions) that match `head`, the committed ones. */
	private async pruneEntry(entry: Ownership["files"][string], head: string): Promise<Ownership["files"][string]> {
		const marks = toMarks(entry.ranges, await this.lines(entry.blob));
		const keep: Marks = { lines: marks.lines.map(() => undefined), gaps: marks.gaps.map(() => undefined) };
		for (const [, , ns, nc] of await blobHunks(this.repo, head, entry.blob)) {
			if (nc === 0) keep.gaps[ns] = marks.gaps[ns];
			for (let k = ns; k < ns + nc; k++) keep.lines[k] = marks.lines[k];
		}
		return { blob: entry.blob, ranges: toRanges(keep) };
	}

	/** Whether `path` is under `<repo>/.pi/`, pi's own state rather than the agent's work. */
	private isPiState(path: string): boolean {
		return path.startsWith(join(this.repo, ".pi") + sep);
	}

	guidelines(): string | undefined {
		return existsSync(this.dir) ? GUIDELINES : undefined;
	}

	private enqueue(name: string): void {
		if (!name.endsWith(".json") || this.seen.has(name)) return;
		this.seen.add(name);
		this.queue = this.queue.then(() => this.handle(join(this.outbox, name))).catch(() => undefined);
	}

	private async handle(path: string): Promise<void> {
		let batch: ReviewBatch;
		try {
			batch = await ReviewBatch.read(path);
		} catch (error) {
			if (error instanceof BatchError) return;
			throw error;
		}
		// A batch routed to its owner keeps the original name before its `.<label>` suffix.
		const routed = basename(path, ".json");
		const name = batch.target && routed.endsWith(`.${batch.target}`) ? routed.slice(0, -batch.target.length - 1) : routed;
		const route = batch.target === undefined ? await this.route(path, batch) : "default";
		if (route === "routed") return;
		if (route === "default" && (await this.addressee(batch)) !== this.id) {
			// Another session's: look again if it is still there when the outbox changes.
			this.seen.delete(basename(path));
			return;
		}
		// Claim it: of several sessions that agree it is theirs, the first rename wins.
		const claimed = path.replace(/\.json$/, `.${this.id}.claimed`);
		try {
			await rename(path, claimed);
		} catch (error) {
			if ((error as NodeJS.ErrnoException).code === "ENOENT") return;
			throw error;
		}
		path = claimed;
		// Another checkout's Helix (or a test) may have used this mailbox: such a batch is not for this agent.
		const paths = [...batch.files, ...batch.comments].map((entry) => entry.path);
		const inRepo = await Promise.all(paths.map(async (p) => (await realpath(p).catch(() => p)).startsWith(this.repo + sep)));
		if (paths.length > 0 && !inRepo.includes(true)) {
			console.warn(`helix-review: dropped ${q(path)}: none of its files is in ${q(this.repo)}`);
			await unlink(path);
			return;
		}
		const baselines = new Map<string, Baseline>();
		for (const { path, baseline } of [...batch.files, ...batch.comments]) {
			if (!baselines.has(path) && !this.isPiState(path)) baselines.set(path, baseline);
		}
		const files = [];
		for (const [file, baseline] of baselines) {
			const current = (await readCurrent(file)).text ?? "";
			const snapshot = this.state.snapshots[file];
			const against = snapshot ? "turn-snapshot" : "baseline";
			const oldOid = snapshot ?? baseline.blobOid ?? (await git(this.repo, ["hash-object", "-w", "--stdin"], "")).trim();
			const old = await git(this.repo, ["cat-file", "blob", oldOid]);
			let hunks: string[] = [];
			if (old !== current) {
				const newOid = (await git(this.repo, ["hash-object", "-w", "--no-filters", "--stdin"], current)).trim();
				const diff = await git(this.repo, ["diff", "--no-color", "--no-ext-diff", "--no-textconv", oldOid, newOid]);
				hunks = diff.split(/^(?=@@ )/m).slice(1);
			}
			const reviewComments = batch.comments
				.filter((comment) => comment.path === file)
				.map((c) => ({
					side: c.side,
					start_line: c.startLine,
					end_line: c.endLine,
					current_line: c.currentLine,
					selected_text: c.selectedText,
					context_before: c.contextBefore,
					context_after: c.contextAfter,
					...(c.enclosingText === undefined ? {} : { enclosing_text: c.enclosingText }),
					comment: c.comment,
				}));
			if (hunks.length || reviewComments.length) {
				files.push({ path: file, user_edits: { against, ...capHunks(hunks) }, review_comments: reviewComments });
			}
		}
		const request = batch.request;
		// With other sessions' hunks staged too, this session commits only its own, from a patch.
		const own = request?.kind === "commit-staged" ? await this.ownCommitPatch(name) : undefined;
		if (files.length > 0 || request) {
			const details = {
				format: "helix-review-handoff",
				version: 1,
				baseline: this.state.baseline?.commit_oid ?? null,
				files,
				...(request ? { request } : {}),
				...(own ? { commit_patch: own } : {}),
			};
			const content = [
				`The user reviewed your changes in Helix and handed back ${files.length} file(s).`,
				"Re-read those files from disk and act on the handoff below.",
				...quote("Handoff", JSON.stringify(details, null, 2)),
				"Integrate the user's edits and never revert them. Then continue editing or committing normally; that refreshes the review in Helix.",
				...(own && request?.kind === "commit-staged" ? patchSection(own, request.subject) : request ? requestSection(request, batch.comments) : []),
			].join("\n");
			this.pi.sendMessage(
				{ customType: "helix-review:handoff", content, display: true, details },
				{ triggerTurn: true, deliverAs: "steer" },
			);
		}
		if (request?.kind === "commit-staged") {
			this.state.commit_requested = await git(this.repo, ["rev-parse", "HEAD"]).then((out) => out.trim(), () => true);
		}
		await this.save();
		await unlink(path);
		await this.echo(`${this.label} received your review: ${receipt(files, request)}`);
	}

	/**
	 * Routes a batch without a target by the owners of the hunks its comments are on (and, for a
	 * commit request, of the first staged hunk): "mine" when they are all this session, else
	 * rewrites it as one batch per owning session, each targeted at it ("routed"). "default"
	 * leaves the usual choice of session.
	 */
	private async route(path: string, batch: ReviewBatch): Promise<"routed" | "mine" | "default"> {
		const inRepo = await Promise.all(
			[...batch.files, ...batch.comments].map(async (e) => (await realpath(e.path).catch(() => e.path)).startsWith(this.repo + sep)),
		);
		if (inRepo.length > 0 && !inRepo.includes(true)) return "default";
		const sessions = await this.liveSessions();
		const ownership = await this.readLedger();
		const labels = batch.comments.map((c) => this.liveLabel(ownerAt(ownership, c.path, c.currentLine), sessions) ?? this.lastEditor(c.path, sessions));
		const committer = batch.request?.kind === "commit-staged" ? [...(await this.stagedOwners(sessions, ownership)).keys()][0] : undefined;
		const owners = [...new Set([...labels, ...(committer ? [committer] : [])])];
		if (owners.length === 0) return "default";
		if (owners.length === 1 && owners[0] === this.label) return "mine";
		// Of several sessions routing it, the first rename wins.
		const routing = path.replace(/\.json$/, `.${this.id}.routing`);
		try {
			await rename(path, routing);
		} catch (error) {
			if ((error as NodeJS.ErrnoException).code === "ENOENT") return "routed";
			throw error;
		}
		const raw = JSON.parse(await readFile(routing, "utf8")) as { comments: unknown[]; files: unknown[]; request?: { kind: string } };
		for (const owner of owners) {
			const doc = { ...raw, comments: raw.comments.filter((_, i) => labels[i] === owner), target: owner };
			// One commit request, to the owner of the first staged hunk; the rest follow in turn.
			if (raw.request?.kind === "commit-staged" && owner !== committer) delete doc.request;
			if (doc.comments.length === 0 && !doc.request && raw.files.length === 0) continue;
			const split = path.replace(/\.json$/, `.${owner}.json`);
			await writeFile(`${split}.tmp`, JSON.stringify(doc));
			await rename(`${split}.tmp`, split);
		}
		await unlink(routing);
		return "routed";
	}

	/** `label` when a live session has it. */
	private liveLabel(label: string | undefined, sessions: Session[]): string | undefined {
		return label && sessions.some((s) => s.state.label === label) ? label : undefined;
	}

	/** The live session that last changed `path` in a turn, else the least busy one. */
	private lastEditor(path: string, sessions: Session[]): string {
		const edited = (s: Session) => s.state.edited?.[path] ?? 0;
		const editor = sessions.reduce((a, b) => (edited(b) > edited(a) ? b : a));
		return (edited(editor) > 0 ? editor : this.idlest(sessions)).state.label!;
	}

	/**
	 * The staged hunks (`git diff --cached`) grouped by the live session owning them in the ledger,
	 * in order of first appearance; hunks without a live owner go to the least busy session.
	 */
	private async stagedOwners(sessions: Session[], ownership: Ownership): Promise<Map<string, StagedHunk[]>> {
		const groups = new Map<string, StagedHunk[]>();
		const names = (await git(this.repo, ["diff", "--cached", "--name-only", "-z"])).split("\0").filter(Boolean);
		const empty = (await git(this.repo, ["hash-object", "-w", "--stdin"], "")).trim();
		for (const name of names) {
			const path = join(this.repo, name);
			if (this.isPiState(path)) continue;
			const head = await this.headBlob(path);
			const index = await git(this.repo, ["rev-parse", "--verify", "--quiet", `:${name}`]).then((out) => out.trim() || empty, () => empty);
			const entry = ownership.files[path];
			// Index lines map to the ledger's (worktree) lines through the unstaged changes.
			const unstaged = entry ? await blobHunks(this.repo, index, entry.blob) : [];
			const toWorktree = (line: number) => {
				let shift = 0;
				for (const [os, oc, ns, nc] of unstaged) {
					if (os + oc <= line) shift += nc - oc;
					else if (os <= line) return ns;
				}
				return line + shift;
			};
			for (const [os, oc, ns, nc] of await blobHunks(this.repo, head, index)) {
				const start = toWorktree(ns) + 1;
				const ranges = (entry?.ranges ?? []).filter((r) =>
					nc === 0 ? r.start === start || (r.start <= start && start < r.end) : r.start < start + nc && start < r.end,
				);
				const last = ranges.sort((a, b) => b.at - a.at)[0];
				const agent = last && (last.owner === "user" ? last.agent : last.owner);
				const owner = this.liveLabel(agent, sessions) ?? this.lastEditor(path, sessions);
				groups.set(owner, [...(groups.get(owner) ?? []), { name, head, index, os, oc, ns, nc }]);
			}
		}
		return groups;
	}

	/**
	 * When the index also holds other sessions' hunks, writes this session's into
	 * `commits/<batch>-<label>.patch` (against `HEAD`) and returns it, marking the commit as a link
	 * in a chain the next owner continues.
	 */
	private async ownCommitPatch(batch: string): Promise<CommitPatch | undefined> {
		const groups = await this.stagedOwners(await this.liveSessions(), await this.readLedger());
		const mine = groups.get(this.label);
		if (!mine || groups.size < 2) return undefined;
		const patch = join(this.dir, "commits", `${batch}-${this.label}.patch`);
		await mkdir(dirname(patch), { recursive: true });
		await writeFile(patch, await this.patchOf(mine));
		this.state.commit_chain = true;
		return {
			patch,
			hunks: mine.map((h) => (h.nc === 0 ? `${h.name}:${h.ns} (lines removed after it)` : `${h.name}:${h.ns + 1}-${h.ns + h.nc}`)),
		};
	}

	/** A unified diff, applying to `HEAD`, of only `hunks`. */
	private async patchOf(hunks: StagedHunk[]): Promise<string> {
		let patch = "";
		for (const name of [...new Set(hunks.map((h) => h.name))]) {
			const own = hunks.filter((h) => h.name === name);
			const split = (text: string) => text.match(/[^\n]*\n|[^\n]+$/g) ?? [];
			const lines = split(await git(this.repo, ["cat-file", "blob", own[0].head]));
			const index = split(await git(this.repo, ["cat-file", "blob", own[0].index]));
			for (const h of [...own].sort((a, b) => b.os - a.os)) lines.splice(h.os, h.oc, ...index.slice(h.ns, h.ns + h.nc));
			const blob = (await git(this.repo, ["hash-object", "-w", "--stdin"], lines.join(""))).trim();
			const diff = await git(this.repo, ["diff", "--no-color", "--no-ext-diff", "--no-textconv", own[0].head, blob]);
			const inHead = await git(this.repo, ["rev-parse", "--verify", "--quiet", `HEAD:${name}`]).then(() => true, () => false);
			// shortcut: a new file is always mode 100644, and removing a whole file stays a modification.
			const header = inHead ? `--- a/${name}\n` : "new file mode 100644\n--- /dev/null\n";
			patch += `diff --git a/${name} b/${name}\n${header}+++ b/${name}\n${diff.slice(diff.indexOf("@@"))}`;
		}
		return patch;
	}

	/** After this session committed its share, asks the owner of the first remaining staged hunk to commit. */
	private async passCommitOn(): Promise<void> {
		const groups = await this.stagedOwners(await this.liveSessions(), await this.readLedger());
		const next = [...groups.keys()].find((label) => label !== this.label);
		if (!next) return;
		const batch = join(this.outbox, `${Date.now()}-${process.pid}-commit.json`);
		const doc = { format: "helix-review-batch", version: 2, files: [], comments: [], request: { kind: "commit-staged" }, target: next };
		await writeFile(`${batch}.tmp`, JSON.stringify(doc));
		await rename(`${batch}.tmp`, batch);
		await this.echo(`${this.label} committed its hunks; ${next} commits the rest`);
	}

	private async save(): Promise<void> {
		const file = join(this.dir, "state", `${this.id}.json`);
		await writeFile(`${file}.tmp`, `${JSON.stringify(this.state, null, 2)}\n`);
		await rename(`${file}.tmp`, file);
	}
}

function socketId(info: { ino: number; ctimeMs: number } | undefined): string | undefined {
	return info && `${info.ino}:${info.ctimeMs}`;
}

function readState(path: string): Promise<HookState | undefined> {
	return readFile(path, "utf8")
		.then((text) => JSON.parse(text) as HookState)
		.catch(() => undefined);
}

/** Whether process `pid` is running. */
function alive(pid: number | undefined): boolean {
	if (!pid) return false;
	try {
		process.kill(pid, 0);
		return true;
	} catch (error) {
		return (error as NodeJS.ErrnoException).code === "EPERM";
	}
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

/** Summarizes a handoff for the reviewer, e.g. "1 file with edits, 2 review comments". */
function receipt(
	files: { user_edits: { hunks: string[] }; review_comments: unknown[] }[],
	request?: ReviewRequest,
): string {
	const count = (n: number, one: string, many: string) => (n > 0 ? [`${n} ${n === 1 ? one : many}`] : []);
	const parts = [
		...(request ? [{ "commit-staged": "commit request", message: "message", ask: "question" }[request.kind]] : []),
		...count(files.filter((file) => file.user_edits.hunks.length > 0).length, "file with edits", "files with edits"),
		...count(files.reduce((n, file) => n + file.review_comments.length, 0), "review comment", "review comments"),
	];
	return parts.length > 0 ? parts.join(", ") : "nothing to do";
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

/** Sends `commands` to Helix's socket. Returns false only when no Helix listens there. */
function helix(path: string, commands: string[]): Promise<boolean> {
	return new Promise((done) => {
		let replies = 0;
		let pending = "";
		let reachable = true;
		const socket = connect(path);
		const finish = () => {
			socket.destroy();
			done(reachable);
		};
		socket.setTimeout(2000, finish);
		socket.on("error", (error: NodeJS.ErrnoException) => {
			reachable = error.code !== "ENOENT" && error.code !== "ECONNREFUSED";
			finish();
		});
		socket.on("close", finish);
		socket.on("connect", () => socket.write(commands.map((command) => `${command}\n`).join("")));
		socket.on("data", (chunk) => {
			const lines = (pending + chunk.toString("utf8")).split("\n");
			pending = lines.pop()!;
			replies += lines.length;
			if (replies >= commands.length) finish();
		});
	});
}

function busyReason(ctx: ExtensionCommandContext): string | undefined {
	if (!ctx.isIdle()) return "pi is busy";
	if (ctx.hasPendingMessages()) return "pi has queued messages";
	return undefined;
}

export default function helixReview(pi: ExtensionAPI): void {
	let importing = false;
	// The cwd's hook, and one per other repository (a worktree, say) the session edits in.
	let hook: Hook | undefined;
	let others = new Map<string, Hook>();
	let sessionId = "";
	let generation = 0;
	const all = () => [...(hook ? [hook] : []), ...others.values()];

	/** The hook of the repository at `dir`, opened and joined into the running turn if new. */
	const hookFor = async (dir: string): Promise<Hook | undefined> => {
		const top = await git(dir, ["rev-parse", "--show-toplevel"]).then((out) => realpath(out.trim()), () => undefined);
		if (!top) return undefined;
		if (top === hook?.repo) return hook;
		let other = others.get(top);
		if (other) return other;
		const opened = generation;
		other = await Hook.open(pi, top, sessionId);
		if (!other || opened !== generation || others.has(top)) {
			await other?.stop();
			return others.get(top);
		}
		others.set(top, other);
		await other.start();
		await other.begin(true);
		await other.echo(`${other.label}: Agent turn started`);
		return other;
	};

	pi.on("session_start", async (_event, ctx) => {
		generation++;
		await Promise.all(all().map((h) => h.stop()));
		others = new Map();
		sessionId = ctx.sessionManager.getSessionId();
		hook = await Hook.open(pi, ctx.cwd, sessionId);
		await hook?.start();
		if (await hook?.attach()) await hook?.announceAttach();
	});
	pi.on("session_shutdown", async () => {
		await Promise.all(all().map((h) => h.stop()));
	});
	pi.on("before_agent_start", (event) => {
		const guidelines = all()
			.map((h) => h.guidelines())
			.find(Boolean);
		if (guidelines) event.systemPromptOptions.promptGuidelines.push(guidelines);
	});
	pi.on("agent_start", async () => {
		for (const h of all()) {
			await h.begin();
			await h.echo(`${h.label}: Agent turn started`);
		}
	});
	pi.on("tool_result", async (event, ctx) => {
		if (event.toolName !== "edit" && event.toolName !== "write" && event.toolName !== "bash") return;
		hook?.refreshSoon();
		const path = event.input.path ?? event.input.file_path;
		if (event.toolName !== "bash" && typeof path === "string" && hook) {
			const file = resolve(ctx.cwd, path.replace(/^~(?=$|\/)/, homedir()));
			const owner = await hookFor(dirname(file));
			owner?.refreshSoon();
			await owner?.attribute([await realpath(file).catch(() => file)], owner.label);
		} else {
			for (const other of others.values()) other.refreshSoon();
			// Bash may have changed anything: what changed is this session's.
			for (const h of all()) await h.attribute(undefined, h.label);
		}
	});
	pi.on("agent_end", async () => {
		// Bash may have changed any worktree of the repository: those with changes join in.
		if (hook) {
			const list = await git(hook.repo, ["worktree", "list", "--porcelain"]).catch(() => "");
			for (const [, dir] of list.matchAll(/^worktree (.+)$/gm)) {
				const top = await realpath(dir).catch(() => undefined);
				if (!top || top === hook.repo || others.has(top)) continue;
				const status = await git(top, ["status", "--porcelain"]).catch(() => "");
				if (status.trim()) await hookFor(top);
			}
		}
		for (const h of all()) {
			const committed = await h.followRequestedCommit();
			await h.refresh();
			await h.endTurn();
			await h.snapshot();
			await h.announceEnd(committed, h !== hook);
		}
	});

	pi.registerCommand("helix-review", {
		description: "Send a Helix :review-export batch to this idle session",
		handler: async (args, ctx) => {
			const arg = args.trim().replace(/^(["'])(.*)\1$/s, "$2");
			if (!arg) return ctx.ui.notify("Usage: /helix-review <batch-file>", "warning");
			const busy = importing ? "another /helix-review import is running" : busyReason(ctx);
			if (busy) return ctx.ui.notify(`helix-review: ${busy}; nothing was sent. Run it again once pi is idle.`, "error");

			importing = true;
			try {
				const sessionId = ctx.sessionManager.getSessionId();
				const batchPath = resolve(ctx.cwd, arg.replace(/^~(?=$|\/)/, homedir()));
				let batch: ReviewBatch;
				let handoff: { message: string; warnings: string[] };
				try {
					batch = await ReviewBatch.read(batchPath);
					handoff = batch.render(batchPath, await batch.inspect());
				} catch (error) {
					if (!(error instanceof BatchError)) throw error;
					return ctx.ui.notify(`helix-review: rejected ${q(batchPath)}: ${error.message}. Nothing was sent.`, "error");
				}

				// The command may have awaited across a session switch, reload, or a newly started turn.
				let drift: string | undefined;
				try {
					drift = ctx.sessionManager.getSessionId() === sessionId ? busyReason(ctx) : "the session changed";
				} catch {
					drift = "the session was replaced or reloaded";
				}
				if (drift) {
					const message = `helix-review: ${drift} during the import; nothing was sent. Run it again in the intended idle session.`;
					try {
						ctx.ui.notify(message, "error");
					} catch {
						throw new Error(message);
					}
					return;
				}

				pi.sendUserMessage(handoff.message);
				const warned = handoff.warnings.length;
				ctx.ui.notify(
					`helix-review: submitted ${batch.files.length} file(s) and ${batch.comments.length} comment(s)${warned > 0 ? ` with ${warned} warning(s)` : ""}.`,
					warned > 0 ? "warning" : "info",
				);
			} finally {
				importing = false;
			}
		},
	});
}
