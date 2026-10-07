import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, type FSWatcher, watch } from "node:fs";
import { mkdir, readdir, readFile, realpath, stat, unlink, writeFile } from "node:fs/promises";
import { connect } from "node:net";
import { homedir } from "node:os";
import { basename, dirname, isAbsolute, join, resolve } from "node:path";
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

class ReviewBatch {
	files: ReviewFile[];
	comments: ReviewComment[];

	constructor(files: ReviewFile[], comments: ReviewComment[]) {
		this.files = files;
		this.comments = comments;
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
				comment: text(entry.comment, `${at}.comment`),
			};
			if (comment.endLine < comment.startLine) throw new BatchError(`${at}.end_line is before start_line`);
			if (comment.side === "old" && comment.baseline.kind === "git-commit" && comment.baseline.blobOid === null) {
				throw new BatchError(`${at} cannot select old text from an absent file`);
			}
			return comment;
		});

		if (files.length === 0 && comments.length === 0) throw new BatchError("it has no files or comments");
		return new ReviewBatch(files, comments);
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
			{ env, encoding: "utf8", timeout: 10_000, maxBuffer: 1024 * 1024, windowsHide: true },
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

interface HookState {
	baseline?: { commit_oid: string; acknowledged: boolean };
	snapshots: Record<string, string>;
}

const GUIDELINES = `## Helix review handoff
The user reviews your changes in Helix against the commit at the start of your turn and hands them back by saving. A handoff arrives as a "helix-review:handoff" message with a JSON block: per file, user_edits are the user's edits as unified-diff hunks (integrate them, never revert them), agent_comments are lines containing "AGENT: <request>" that the user left for you (new: true when added in this review), and review_comments are inline review comments. Address each AGENT: comment and delete its marker line or comment once handled.`;

class Hook {
	private pi: ExtensionAPI;
	readonly repo: string;
	readonly dir: string;
	private state: HookState;
	private watcher?: FSWatcher;
	private timer?: NodeJS.Timeout;
	private queue = Promise.resolve();
	private seen = new Set<string>();

	constructor(pi: ExtensionAPI, repo: string, dir: string, state: HookState) {
		this.pi = pi;
		this.repo = repo;
		this.dir = dir;
		this.state = state;
	}

	static async open(pi: ExtensionAPI, cwd: string): Promise<Hook | undefined> {
		let repo: string;
		try {
			repo = await realpath((await git(cwd, ["rev-parse", "--show-toplevel"])).trim());
		} catch {
			return undefined;
		}
		const dir = join(repo, ".pi", "helix-review");
		await mkdir(join(dir, "outbox"), { recursive: true });
		if (!existsSync(join(dir, ".gitignore"))) await writeFile(join(dir, ".gitignore"), "*\n");
		const state: HookState = await readFile(join(dir, "state.json"), "utf8")
			.then((text) => JSON.parse(text) as HookState)
			.catch(() => ({ snapshots: {} }));
		return new Hook(pi, repo, dir, state);
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

	start(): void {
		this.watcher = watch(this.outbox, (_event, name) => name && this.enqueue(name));
		void readdir(this.outbox).then((names) => names.forEach((name) => this.enqueue(name)));
	}

	stop(): void {
		this.watcher?.close();
		clearTimeout(this.timer);
	}

	async begin(): Promise<void> {
		if (!this.state.baseline || this.state.baseline.acknowledged) {
			const head = (await git(this.repo, ["rev-parse", "HEAD"])).trim();
			this.state.baseline = { commit_oid: head, acknowledged: false };
			await this.save();
		}
		await this.attach();
	}

	async attach(): Promise<void> {
		if (this.state.baseline) {
			await helix(this.socket, [`review-handoff ${this.outbox}`, `review-baseline ${this.state.baseline.commit_oid}`]);
		}
	}

	refreshSoon(): void {
		clearTimeout(this.timer);
		this.timer = setTimeout(() => void this.refresh(), 200);
	}

	async refresh(): Promise<void> {
		clearTimeout(this.timer);
		if (this.state.baseline) await helix(this.socket, [`review-baseline ${this.state.baseline.commit_oid}`]);
	}

	async snapshot(): Promise<void> {
		if (!this.state.baseline) return;
		const changed = await git(this.repo, ["diff", "--name-only", "-z", this.state.baseline.commit_oid]);
		const untracked = await git(this.repo, ["ls-files", "--others", "--exclude-standard", "-z"]);
		const paths: string[] = [];
		for (const name of new Set(`${changed}${untracked}`.split("\0").filter(Boolean))) {
			const path = join(this.repo, name);
			if (!path.startsWith(this.dir) && (await stat(path).catch(() => undefined))?.isFile()) paths.push(path);
		}
		const oids = paths.length ? (await git(this.repo, ["hash-object", "-w", "--no-filters", "--", ...paths])).split("\n") : [];
		this.state.snapshots = Object.fromEntries(paths.map((path, i) => [path, oids[i]]));
		await this.save();
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
		const baselines = new Map<string, Baseline>();
		for (const { path, baseline } of [...batch.files, ...batch.comments]) if (!baselines.has(path)) baselines.set(path, baseline);
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
			const oldLines = new Set(old.split("\n"));
			const agentComments = current.split("\n").flatMap((line, i) => {
				const marker = /\bAGENT:\s*(.+)$/.exec(line);
				return marker ? [{ line: i + 1, text: marker[1], new: !oldLines.has(line) }] : [];
			});
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
					comment: c.comment,
				}));
			if (hunks.length || agentComments.length || reviewComments.length) {
				files.push({ path: file, user_edits: { against, hunks }, agent_comments: agentComments, review_comments: reviewComments });
			}
		}
		if (files.length > 0) {
			const details = {
				format: "helix-review-handoff",
				version: 1,
				baseline: this.state.baseline?.commit_oid ?? null,
				files,
			};
			const content = [
				`The user reviewed your changes in Helix and saved ${files.length} file(s).`,
				"Re-read those files from disk and act on the handoff below.",
				...quote("Handoff", JSON.stringify(details, null, 2)),
				"Integrate the user's edits and never revert them. Address each AGENT: comment and delete its marker line or comment once handled. Then continue editing or committing normally; that refreshes the review in Helix.",
			].join("\n");
			this.pi.sendMessage(
				{ customType: "helix-review:handoff", content, display: true, details },
				{ triggerTurn: true, deliverAs: "steer" },
			);
		}
		if (this.state.baseline) this.state.baseline.acknowledged = true;
		await this.save();
		await unlink(path);
	}

	private save(): Promise<void> {
		return writeFile(join(this.dir, "state.json"), `${JSON.stringify(this.state, null, 2)}\n`);
	}
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

function helix(path: string, commands: string[]): Promise<string[] | undefined> {
	return new Promise((done) => {
		const replies: string[] = [];
		let pending = "";
		const socket = connect(path);
		const finish = () => {
			socket.destroy();
			done(replies.length === commands.length ? replies : undefined);
		};
		socket.setTimeout(2000, finish);
		socket.on("error", finish);
		socket.on("close", finish);
		socket.on("connect", () => socket.write(commands.map((command) => `${command}\n`).join("")));
		socket.on("data", (chunk) => {
			const lines = (pending + chunk.toString("utf8")).split("\n");
			pending = lines.pop()!;
			replies.push(...lines);
			if (replies.length >= commands.length) finish();
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
	let hook: Hook | undefined;

	pi.on("session_start", async (_event, ctx) => {
		hook?.stop();
		hook = await Hook.open(pi, ctx.cwd);
		hook?.start();
		await hook?.attach();
	});
	pi.on("session_shutdown", () => hook?.stop());
	pi.on("before_agent_start", (event) => {
		const guidelines = hook?.guidelines();
		if (guidelines) event.systemPromptOptions.promptGuidelines.push(guidelines);
	});
	pi.on("agent_start", () => hook?.begin());
	pi.on("tool_result", (event) => {
		if (event.toolName === "edit" || event.toolName === "write" || event.toolName === "bash") hook?.refreshSoon();
	});
	pi.on("agent_end", async () => {
		await hook?.refresh();
		await hook?.snapshot();
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
