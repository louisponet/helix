# Helix review batch schema (version 2)

Helix writes a review batch with `:review-export <path>`. The batch is a single
UTF-8 JSON document. A consumer (for example the `/helix-review <file>` pi
command) reads it once, in the pi session the user explicitly chose; Helix
never notifies or wakes pi itself.

Each file and each comment names its own **baseline**, the old snapshot the
saved document was compared against:

- `git-index`: the file's staged content (`:review`).
- `git-commit`: the file as it was in a commit (`:review <revision>` or
  `:review-commits`). The chosen commit is the before-state, so the review
  shows every change since it: reviewing against `HEAD~1` covers the latest
  commit plus any staged and unstaged edits. Helix resolves the revision once
  and pins the full commit id, also across `:review-refresh`, so a commit
  baseline stays valid when `HEAD`, the branch, or the index move later. A path
  that does not exist in that commit has an empty old snapshot: all current
  text is added. Renames are not followed.

The "old" side of a review is the baseline blob converted to worktree form with
the same filters Helix uses for its diff gutter; the "current" side is the
Helix document, which `:review-export` requires to be saved. `blob_oid`
identifies the raw blob; because of that conversion (for example end-of-line
handling) the old text Helix shows and exports may not be byte-identical to it.
The exported `selected_text` and context are the authoritative snapshot text.

One batch can mix baselines: different files can use different baselines, and
a comment keeps the baseline it was made against when the user later switches
the file to another baseline, so a comment's baseline may differ from its
file's.

## Handoff flow

1. Pause pi (and any agents it spawned) so the working tree stops changing.
2. In Helix: `:review` (or `:review <revision>`, `:review-commits`) each file,
   edit/comment, save.
3. `:review-export <path>` writes the batch. Export refuses to run while any
   reviewed buffer has unsaved changes, refuses to write over an existing file,
   and keeps pending comments if writing fails. On success the pending comments
   are cleared. These are Helix-side behaviours; a consumer should not rely on
   them and should treat files and metadata as possibly changed since export.
4. In the idle pi session that should resume: `/helix-review <path>`.

## Document

```json
{
  "format": "helix-review-batch",
  "version": 2,
  "files": [
    {
      "path": "/abs/repo/src/lib.rs",
      "baseline": {
        "kind": "git-commit",
        "commit_oid": "8f2c1d0e4b7a9c3e5f6a1b2c3d4e5f6a7b8c9d0e",
        "blob_oid": "3b18e512dba79e4c8300dd08aeb37f8e728b8dad"
      }
    },
    {
      "path": "/abs/repo/src/new.rs",
      "baseline": {
        "kind": "git-commit",
        "commit_oid": "8f2c1d0e4b7a9c3e5f6a1b2c3d4e5f6a7b8c9d0e",
        "blob_oid": null
      }
    }
  ],
  "comments": [
    {
      "path": "/abs/repo/src/lib.rs",
      "side": "old",
      "baseline": {
        "kind": "git-index",
        "blob_oid": "c2a6b03f190dfb2b4aa91f8af8d477a9bc3401dc"
      },
      "start_line": 12,
      "end_line": 13,
      "current_line": 11,
      "selected_text": "fn removed() {\n}\n",
      "context_before": "// helpers\n",
      "context_after": "\nfn kept() {}\n",
      "comment": "Why was this removed?"
    }
  ]
}
```

Consumers must reject documents whose `format` is not `"helix-review-batch"`
or whose `version` they do not support. Unknown extra fields must be ignored.
Version 1 (a single top-level `"baseline": "git-index"` and `index_oid`
fields) is not accepted by version 2 consumers; re-export the review with an
updated Helix.

### Top level

| Field      | Type   | Meaning |
|------------|--------|---------|
| `format`   | string | Always `"helix-review-batch"`. |
| `version`  | number | Schema version, `2`. |
| `files`    | array  | Every file reviewed since the previous export (including files without comments and reviews closed since), in no particular order. Each path appears once. |
| `comments` | array  | Pending comments in the order they were made. May be empty. |
| `request`  | object | Optional. What the reviewer asks the agent to do besides the edits and comments: `{ "kind": "commit-staged", "subject"?: string }` (from `:review-commit [subject]`: commit exactly what is staged in the index) or `{ "kind": "message", "text": string }` (from `:review-send <text>`) or `{ "kind": "ask" }` (from `:review-ask <text>`: act on the batch's comments, one per selection, the primary selection's first). A batch with a request may have no files or comments. |
| `target`   | string | Optional. The pi session the batch is for, by session id or label (`pi-2`); only that session takes it. Without it, the hook gives the batch to the session that last changed one of its files, else the most recently active one. |

There is no batch-wide baseline.

### `files[]`

| Field      | Type   | Meaning |
|------------|--------|---------|
| `path`     | string | Absolute path of the reviewed file. Its saved on-disk content is the reviewed current side; consumers should reread it. |
| `baseline` | object | The latest baseline the file was reviewed against, at export time. See below. |

### `baseline`

| `kind`         | Other fields | Meaning |
|----------------|--------------|---------|
| `"git-index"`  | `blob_oid`: string | Hex object id of the raw index blob. |
| `"git-commit"` | `commit_oid`: string, `blob_oid`: string or `null` | `commit_oid` is the full hex id (40 or 64 digits) of the resolved commit, never a branch, tag or abbreviated name. `blob_oid` is the hex id of the raw blob at the file's path in that commit, or `null` when the path does not exist there (empty old snapshot). |

Object ids are full hex, 40 digits (SHA-1) or 64 (SHA-256). For a symlink, the entry is that of the link's target, as Helix
reviews it. A commit baseline is immutable: a consumer may verify that
`commit_oid` still names a commit whose entry at the path matches `blob_oid`
(or is absent for `null`), but it must not compare it with the current index or
`HEAD`. An index baseline can be compared with the current index entry to
detect later staging.

### `comments[]`

| Field            | Type   | Meaning |
|------------------|--------|---------|
| `path`           | string | Absolute path of the file the comment belongs to. |
| `side`           | string | `"old"`: the selection is text from the baseline snapshot (typically deleted or replaced lines). `"current"`: the selection is text from the Helix document. |
| `baseline`       | object | The baseline in effect when the comment was made, in the same form as `files[].baseline`. It is kept when the review is later refreshed, switched to another baseline, or closed, so it may differ from the file's `baseline` in `files`. |
| `start_line`     | number | 1-based first line of the selection, in the text of `side` at comment time. A hint only. |
| `end_line`       | number | 1-based last line (inclusive) of the selection, same coordinates. A hint only. |
| `current_line`   | number | 1-based line in the current text at comment time where the selection sits. For `"current"` it equals `start_line`. For `"old"` it is the current line the old text was removed from: old text lies immediately before this line (one past the last line when it was removed from the end of the file). A hint only. |
| `selected_text`  | string | Exact selected text from the `side` snapshot, including line endings it covers. Authoritative. |
| `context_before` | string | Snapshot text from the start of the line 3 lines above the selection's first line up to the selection start. May be empty. |
| `context_after`  | string | Snapshot text from the selection end through the end of the line 3 lines below the selection's last line (including its line ending). May be empty. |
| `enclosing_text` | string | Optional. For `"current"` comments in a buffer with a syntax tree: the text of the smallest syntax node around the selection whose kind names a function, method, impl, class, struct, enum, trait or module (or, when none does, the smallest one spanning at least three lines), cut at 200 lines. |
| `comment`        | string | The reviewer's comment text. |

An `"old"` comment cannot have a commit baseline with `blob_oid: null`: that
snapshot has no old text to select. Consumers must reject that combination.

Line numbers are hints taken when the comment was made; later edits in Helix are
not mapped onto them. Consumers should locate comments by `selected_text` plus
context, and use the line numbers only to disambiguate repeated matches.
`context_before + selected_text + context_after` is a contiguous slice of the
snapshot.

## Hook-mode handoff message

In hook mode the pi extension turns each batch Helix writes into its outbox
into one custom message (`customType: "helix-review:handoff"`). This is not a
new batch version: the batch above is its input. The message `details`, also
quoted as a fenced JSON block in its content, is:

```json
{
  "format": "helix-review-handoff",
  "version": 1,
  "baseline": "8f2c1d0e4b7a9c3e5f6a1b2c3d4e5f6a7b8c9d0e",
  "files": [
    {
      "path": "/abs/repo/src/lib.rs",
      "user_edits": {
        "against": "turn-snapshot",
        "hunks": ["@@ -3 +3,2 @@\n-fn b() {}\n+fn b() -> u8 { 1 }\n+// AGENT: rename b\n"],
        "truncated": false
      },
      "agent_comments": [{ "line": 4, "text": "rename b", "new": true }],
      "review_comments": [
        {
          "side": "current",
          "start_line": 1,
          "end_line": 1,
          "current_line": 1,
          "selected_text": "fn a() {}",
          "context_before": "",
          "context_after": "\n",
          "comment": "document a"
        }
      ]
    }
  ]
}
```

| Field | Meaning |
|-------|---------|
| `baseline` | The commit the agent's turn was reviewed against, or `null` if pi has none. |
| `files[].path` | Absolute path from the batch (its files and comment paths). |
| `files[].user_edits.against` | `"turn-snapshot"`: the file as the agent left it at the end of its last turn. `"baseline"`: the batch's baseline blob for the file (empty when absent). |
| `files[].user_edits.hunks` | Unified-diff hunks (`git diff` of the raw bytes) from that content to the saved file. Whole hunks are kept up to 200 lines per file; the rest are replaced by one entry such as `"... 3 more hunks (480 lines) omitted; read the file"`. |
| `files[].user_edits.truncated` | `true` when hunks were omitted that way, else `false`. |
| `files[].agent_comments` | Every line of the saved file matching `/\bAGENT:\s*(.+)$/`: 1-based `line`, the request `text`, and `new` when the line is not in the compared-against content. |
| `files[].review_comments` | The batch's comments for the file, with the batch fields except `path` and `baseline`. |
| `request` | The batch's `request`, when it has one. The message content then ends with a "User request" section: for `commit-staged`, an instruction to commit exactly what is staged (`git commit` without `-a`, a Conventional Commit message derived from `git diff --cached`, the subject if given), touch nothing else and report the commit hash; for `message`, the text verbatim; for `ask`, an instruction to do what the message asks, followed by each comment: path, lines, and the fenced selected text, enclosing code (when present) and message. |

Files without edits, markers or comments are omitted; a handoff with no files
and no request is not sent.

After a handoff with a `commit-staged` request, the hook checks `HEAD` at the
end of the turn the handoff starts or steers: if the agent committed, the
review baseline moves to that commit (not yet acknowledged, so it stays until
the next handoff) and Helix is sent `review-baseline <commit>`.
