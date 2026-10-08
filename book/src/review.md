## Inline review

`:review` compares the current buffer with the file's content staged in the git
index, so only unstaged changes are shown, unless an editor-wide
`:review-baseline` is set: then it uses that commit. `:review index` always
uses the index, and `:review <revision>` uses a commit snapshot; `:review-commits` opens a picker of recent commit baselines.
Other review commands do not need `:review` first: in a workspace file without
a review they pin `HEAD` as the editor-wide baseline when none is set (like
`:review-baseline HEAD`) and start the buffer's review against it, unless you
closed it with `:review-close`.
For buffers within the [size limits](#size-limits), a clean buffer is first
reloaded from disk, after the requested baseline is successfully loaded.
Unsaved edits are kept; if the file also changed on disk, review refuses to start.
External changes use the same modification-time check as ordinary write conflicts.
Lines that differ from the baseline are highlighted with `diff.plus.review`.
Old lines that were removed or replaced are shown inline as read-only virtual lines with
`diff.minus.review` and a `-` in the gutter. A background colour on either scope
shades the whole row and leaves syntax colours in place, with the cursorline drawn
on top; themes without one fall back to the `diff.plus` and `diff.minus`
foreground colours. The buffer itself is the ordinary
document: editing, undo, saving, syntax highlighting and language servers work
as usual and never see the old lines. While the review is active, the diff
gutter and the change motions (`]g`, `[g`, `]G`, `[G`, and `n` / `N` while the
search register is empty) compare against the review baseline instead of the
git index, so they follow the review even after its changes are committed.
Closing the review, `:review-baseline clear` or hitting the
[size limits](#size-limits) restores the ordinary diff.

Old lines are shown directly above the lines that replaced them. Lines removed
from the very start of the file cannot be shown above the first line, so they
are shown below the lines that replaced them (or below the first line) under a
label such as `old lines 1-2 removed before line 1:`.

| Command | Description |
| --- | --- |
| `:review` | Start or switch review to the `:review-baseline` commit when one is set, otherwise to the git index. |
| `:review index` | Start or switch review to the git index. Fails for files that are not in the index or have unmerged (conflicted) entries. |
| `:review <revision>` | Start or switch review to a commit, such as `HEAD~1`, a branch, tag or commit hash. |
| `:review-commits` | Choose a baseline by hash or subject from the latest 200 commits reachable from `HEAD`. |
| `:review-old` | Focus the old text nearest the cursor. |
| `:review-refresh` | Reload the buffer from disk; reread the index in index mode, or retain the selected commit in commit mode. Refuses unsaved changes. |
| `:review-comment <text>` | Add a pending comment on the primary selection, or on the selected old text while it is focused. Use `%%` for a literal `%`. |
| `:review-ask <text>` | Ask the agent about every selection, or the selected old text while it is focused: one comment per selection with the text, then hands the review off at once (writing the buffer first if it is modified). Needs `:review-handoff`. Use `%%` for a literal `%`. |
| `:review-reply <file>` | Show the agent's reply from a JSON file `{path, current_line, text}` (written by the pi extension) inline in that buffer's review. Ignored when the buffer is not open or not reviewed. |
| `:review-dismiss [count]` | Hide the latest question with its placeholders and reply, or the `count` latest ones. `:review-close` hides them all. |
| `:review-comments` | Pick a pending comment by file, lines or text and jump to its line in the current text. |
| `:review-uncomment [count]` | Drop the latest pending comment, or the `count` latest ones, and show how many remain. |
| `:review-export <path>` | Write all pending comments and reviewed files to a new JSON file. |
| `:review-revert` | Replace the change under the cursor, or the focused old text's change, with the baseline's lines (added lines are deleted; removed lines shown below the cursor line are restored). One undoable edit; the file on disk is not touched. |
| `:review-stage` | Stage the unstaged hunk (index to worktree) under the cursor in the git index, like `git add -p`: in a reviewed buffer the hunk on the cursor line or of the focused old text, in the `:review-diff` buffer the hunk under the cursor. Refuses buffers with unsaved changes, a locked index (`index.lock`), and files git converts (filters, line endings). |
| `:review-unstage` | Unstage the staged hunk under the cursor (same places as `:review-stage`): the index gets the review baseline's lines back for it, even when that hunk was already committed. |
| `:review-toggle-stage` | Stage the hunk under the cursor, or unstage it if it is already staged, so hunks can be toggled in any order. |
| `Enter` | In normal mode, runs `:review-toggle-stage` while the buffer is reviewed or is the `:review-diff` buffer, and does nothing otherwise. |
| `:review-close` | Stop reviewing the buffer. Pending comments are kept. Under a `:review-baseline`, the buffer is not reviewed again until a different baseline, `:review-baseline clear`, or an explicit `:review` on it. |
| `:review-baseline <revision>` | Review all unmodified workspace buffers, and buffers opened later, against a commit. `clear` closes all reviews. |
| `:review-handoff <dir>` | Export a batch to `<dir>` on every write of a reviewed buffer. `clear` stops it. |
| `:review-send [text]` | Export the handoff batch now without writing any file, for example to acknowledge a review without edits; optional text goes to the agent as an instruction (the batch's `request`). |
| `:review-commit [subject]` | Hand back like `:review-send`, asking the agent to commit exactly what is staged in the git index (optionally with that subject). Refuses when nothing is staged. |
| `:review-diff` | Show one unified diff of every file changed since the review baseline (with none set, `HEAD` is pinned as it, like `:review-baseline HEAD`) in a read-only scratch buffer; running it again, or `:review-baseline` (as the pi hook does after each edit), refreshes that buffer in place. File headers are workspace-relative paths (`--- <path>`), so `gf` opens a file. |
| `:review-diff-open` | In the `:review-diff` buffer, open the file of the hunk under the cursor at the current line it shows (the next current line on a removed line). |
| `:review-files` | Pick a file added (`A`), modified (`M`) or deleted (`D`) since the review baseline, and open it at its first change. The `+/-` and `hunks` columns show added and removed lines and staged/total hunks (`?` over the size limits), and the status line the totals. With no baseline set, it first pins `HEAD` like `:review-baseline HEAD`. |

### Choosing a commit range

The selected commit is the **before** state; the current editable buffer is the
**after** state, including subsequent commits, staged changes and unsaved edits.
For example, `:review HEAD~1` includes the latest commit's changes, while
`:review HEAD~3` compares from three first-parent steps before `HEAD`. To include
a particular commit and everything after it, choose its parent
(`:review <hash>^`).

The picker shows repository history, not only commits touching the current file.
Type a subject or hash to filter, press `Enter` to select, or `Esc` to cancel.
Use `:review <revision>` for commits beyond the picker's 200-entry window.
Review is per buffer: opening another file does not automatically review it,
unless an editor-wide baseline is set with `:review-baseline`.

A revision is resolved to a fixed commit ID. `:review-refresh` keeps that same
commit even after the branch advances; choose another baseline explicitly to
change the range. Missing files in a valid baseline commit are treated as empty,
so files added later can be reviewed. Renames are not followed. Switching
baselines preserves pending comments and their original snapshot metadata.
Use `:review index` to return to unstaged-only review.

### Old text

`:review-old` moves a separate, read-only selection into the old lines. Keys go
through the normal mode keymap, but only these commands run; anything else is
rejected with an error and nothing is changed:

- movement and selection: `h` `j` `k` `l`, `w` `b` `e` `W` `B` `E`, `gh` `gl`,
  `gg` `ge`, `x` `X`, `%` (the whole block), `;`, `Alt-;`, `v` (toggle
  extending) and counts. Selections stay inside one block of old lines.
- `]g` / `[g` / `[G` / `]G`: next, previous, first and last block of old lines.
  Like `:review-old`, they center the block and the lines that replaced it
  unless both are already fully visible.
- `y`, `Space y`, `Space Y`: yank the old selection.
- `:` with only `:review-comment`, `:review-comments`, `:review-uncomment`,
  `:review-export`, `:review-send`, `:review-close`, `:review-old` and
  `:review-revert`, which returns to the current text.
- `Esc` returns to the current text with the cursor on the first line that
  replaced the focused block, or on the line it is shown below if nothing
  replaced it.

Mouse input and pasting are ignored while old text is focused.

For an emptied file, or old lines displayed below a final line without a newline,
normal scrolling stops at the current text's end. Use `:review-old` and old-text
motions (for example `ge` or `j`) to scroll these old lines instead.

### Size limits

Review runs synchronously after edits, so each side is limited to 256 KiB of
decoded UTF-8 text and 10,000 buffer lines (including the empty buffer line
after a final newline). Larger files cannot start review; reduce both sides below
the limits and retry `:review`. If an edit crosses
either limit, the statusline shows **Review suspended**, old lines and review
highlights disappear, and review comments/export are refused. Ordinary editing,
undo and saving still work, and pending comments are kept.

Keep both the buffer and chosen baseline below the limits. If a historical
baseline is too large, choose a smaller one with `:review <revision>`.
Save or undo unsaved edits, then use `:review-refresh` to resume. Review stays
suspended until explicitly refreshed, even after undo. `:review-close` removes
the review and its warning.

The size check runs before automatic reload. If an agent shrinks an oversized
file on disk, run `:reload` first, then `:review-refresh` (or `:review` if review
is not active). Preserve any unsaved edits before `:reload`: it replaces the
buffer with the file on disk.

### Handing a review to pi

The `contrib/pi-helix-review` integration imports exported batches into a pi
session. Nothing is sent automatically; saving or commenting never notifies pi.

1. Pause pi (and any agents it started) so the working tree stops changing.
2. In Helix, `:review` each file (`:review-refresh` picks up changes made since),
   edit and `:write`, and add comments with `:review-comment`. Wait for writing
   to finish before refreshing; chaining both commands in a key mapping does
   not wait for the save.
3. `:review-export <path>` writes the batch. It refuses while a reviewed buffer
   has unsaved changes, changed on disk (use `:review-refresh`), or has suspended
   review. It never writes to an existing file and keeps pending comments on
   failure. On success the pending comments are cleared. Keep reviewed buffers
   open until export. Missing or moved paths must be restored at their original
   locations and reopened before exporting; renames are not tracked, and pending
   comments can only be dropped latest first with `:review-uncomment`.
4. In the idle pi session that should continue, run `/helix-review <path>`.

The batch format is described in `contrib/pi-helix-review/schema.md`. Comments
carry the selected text and its surrounding lines, line numbers are only hints
from the time the comment was made. Pending comments survive `:review-refresh`
and `:review-close` and keep the baseline they were made against, even after
selecting another commit. Export checks buffer/disk freshness, not whether
every new version was inspected: after a reload or reopen, inspect the updated
diff before handing it off.

Commit-aware batches use schema version 2 and require the updated pi extension.
Version 1 batches are rejected; use matching editor and extension versions.

### Reviewing an agent's work

In hook mode (see [Driving Helix from pi](#driving-helix-from-pi)), the pi
extension keeps Helix reviewing whatever the agent changes. A review then goes
like this:

1. Wait for the agent. The status line shows `Agent turn started`, and at the
   end of the turn `Agent turn done: 3 files changed since 1a2b3c4d
   (:review-files)`. Unmodified open buffers in the workspace are already
   reviewed against the baseline commit.
2. `:review-files` picks a file changed since the baseline, listing its added
   and removed lines and staged/total hunks, and opens it with the cursor on
   its first change (or read the whole change as one diff first
   with `:review-diff`, and jump from a hunk into its file with
   `:review-diff-open`). It shows the review summary, for example
   `Reviewing against the git commit 1a2b3c4d: 2 blocks of old text,
   :review-old to inspect them`.
3. `]g` / `[g` move to the next or previous hunk of the current text that
   differs from the baseline, selecting its changed lines; `]G` / `[G` jump to
   the last or first one. The diff gutter marks the same hunks, and both follow
   the review baseline (not the git index) while the review is active, so they
   still work after the agent has committed. Until you search for something,
   `n` / `N` move between hunks too; after a `/` search they repeat it as
   usual, and `:clear-register /` brings the hunk behaviour back.
4. `:review-old` focuses the nearest old lines. Inside them, `]g` / `[g` move to
   the next or previous block of old lines, and `Esc` returns to the current
   text next to the block you were reading.
5. Edit the current text as usual. Old lines are read-only. `:review-revert`
   rejects the change under the cursor (or the focused old text's change) by
   restoring the baseline's lines as one edit; `u` brings the agent's version
   back.
6. To commit part of the change, stage it hunk by hunk: `Enter` (or
   `:review-toggle-stage`) stages the hunk under the cursor in the git index, or
   unstages it if it is already staged, in any order, from a reviewed buffer,
   focused old text or the `:review-diff` buffer, where staged hunks are marked
   `(staged)`. In the buffer a staged hunk's lines turn blue and its old lines
   grey (`diff.plus.review.staged`, `diff.minus.review.staged`) until the next
   edit. Save first: only what is on disk is staged. While reviewing, `Enter` in
   normal mode is taken by this; elsewhere it does nothing.
7. `:review-comment <text>` comments on the current selection, or on the
   selected old text while it is focused. `:review-comments` lists pending
   comments and jumps to one; `:review-uncomment [count]` drops the latest.
   To get an answer right away, select the code (several selections are
   fine) and use `:review-ask <text>` (`G a`): it hands the selections and the
   question to the agent at once, writing the buffer first if needed. Each
   selection shows a grey `you │ <question>` line (`ui.review.ask`) above the
   change closest to it, or above the selection when the buffer has no
   changes. You watch the agent's edits arrive; its short answer then replaces
   the primary selection's line as an `agent │` block (`ui.review.reply`, at
   most 20 rows) above the change closest to where you asked. Answers stay
   through refreshes until `:review-dismiss` (`G D`) or `:review-close`, and the
   statusline counts them.
8. For a request that belongs in the code, write a line containing
   `AGENT: <request>` (for example `// AGENT: handle the empty case`); the
   agent is told to address it and delete the marker.
9. Hand the review back: `:w` on a reviewed buffer exports your edits,
   markers and comments, or `:review-send` exports them without writing
   anything when there is nothing to save; `:review-send <text>` adds an
   instruction for the agent. `:review-commit [subject]` hands back and asks
   the agent to commit exactly what you staged (and nothing else). The status
   line then confirms it, for example `pi received your review: 1 file with
   edits, 1 review comment`.

The baseline stays on the same commit until a handoff is received, so a
following agent turn is still reviewed against the commit you started from.
The first turn after a handoff moves the baseline to the then current `HEAD`.

These commands are built into the default keymap under `G` in normal mode
(see [review mode](./keymap.md#review-mode)): `G f` picks a file, `G d` shows the
diff, `G o` focuses old text, `G m` comments, `G s` stages, `G C` asks the agent
to commit and `G n` / `G N` move between changes. Keys you map under
`[keys.normal.G]` in `config.toml` are added to that menu.

As an alternative, a keymap under `Space r`, for `config.toml`. It replaces
the default `Space r` (rename symbol); pick another key to keep it.

```toml
[keys.normal.space.r]
f = ":review-files"
d = ":review-diff"
g = ":review-diff-open"
o = ":review-old"
c = ":review-comments"
u = ":review-uncomment"
S = ":review-send"
C = ":review-commit"
r = ":review-revert"
s = ":review-toggle-stage"
R = ":review-refresh"
x = ":review-close"
```

`:review-comment` takes the comment text, so type it at the `:` prompt. The
`review` statusline element shows the baseline, the hunk count, how many hunks
are staged and the pending comment count; add it with, for example,
`right = ["review", "diagnostics", "selections", "position"]` under
`[editor.statusline]`.

### Troubleshooting

- No `pi hook attached` status: Helix was not listening when pi started (it
  was started after pi, or in another workspace, which has its own socket).
  Open Helix in the repository and run `/reload` in pi (or restart pi); the
  next agent turn also attaches. Meanwhile `:review-files` reviews against
  `HEAD` on its own, but only pi sets the handoff directory, so saving hands
  nothing back until it attaches.
- `:review-files` says `no files changed since <commit>` although the agent did
  work: it committed, and after your last handoff the next turn moved the
  baseline to that commit. `:review-baseline <commit>` (for example
  `:review-baseline HEAD~1`) re-pins an earlier one, until pi's next refresh
  sends its own baseline again.
- `... also changed on disk: :reload discards your edits, :w! overwrites the
  agent's`: you and the agent edited the same file. `:reload` keeps the
  agent's version, `:w!` keeps yours.
- A stale `:review-diff`: it refreshes whenever `:review-baseline` runs, which
  pi sends after each agent edit; run `:review-diff` to refresh it now.

### Driving Helix from pi

Helix listens on a Unix socket for typed commands, so the pi extension can keep
the review in sync with an agent. The socket is `$HELIX_COMMAND_SOCKET` if set,
otherwise `$XDG_RUNTIME_DIR/helix/<id>.sock` (or `/tmp/helix-<uid>/<id>.sock`),
where `<id>` is the first 32 hex digits of the SHA-256 of the canonical
workspace root. Only the first Helix instance in a workspace listens; the
socket is removed on exit, unless a newer instance has already replaced it. An
instance that found the socket taken retries whenever the editor goes idle, so
it takes over once the other instance exits.

Each line sent is one typed command without the leading `:`, for example
`review-baseline HEAD` or `open /abs/path.rs`. It runs exactly as if typed at
the `:` prompt, and Helix replies with one line per command: `ok`,
`ok <status message>` or `error <message>`. Relative paths resolve against
Helix's working directory.

Any typed command can be run through the socket, including ones that write
files or run shell commands. The socket directory is created with mode `0700`,
so only processes of the same user can connect; do not point
`HELIX_COMMAND_SOCKET` into a directory other users can access.

`:review-baseline <revision>` resolves the revision once and pins the commit
for the editor; while it is set, plain `:review` uses it too. Every unmodified
buffer in the workspace is reloaded from disk and reviewed against it; buffers with unsaved changes are skipped and picked up
on their next write. If such a buffer's file also changed on disk, the status
names it (for example `; review.rs also changed on disk: :reload discards your
edits, :w! overwrites the agent's`): you and the agent edited the same file, so
decide which version wins before writing. Buffers opened or reloaded later are reviewed
automatically, showing the same status as `:review` (or `no changes` when the
buffer matches the baseline). Sending the same commit again refreshes all unmodified reviewed
buffers, keeping any baseline chosen per buffer with `:review`; buffers already
reviewed against that commit whose file has not changed on disk are left alone
and counted as unchanged (`2 buffers up to date against <commit>` when nothing
else happened). A different commit switches all of them.

While `:review-handoff <dir>` is set, auto-save skips reviewed buffers, and
writing a reviewed buffer exports a batch (the same version 2 JSON as
`:review-export`) to `<dir>/<unix-ms>-<pid>.json`. The file is first written
under a `.json.tmp` name and then renamed. The batch covers every unmodified
reviewed buffer with its pending comments; comments on buffers with unsaved
changes stay pending for a later write.

`:review-send` exports the same batch immediately without writing any file, so
a review without edits can be handed back without touching the disk. It fails
when no handoff directory is set, and names the reviewed buffers that have
unsaved changes instead of exporting.
