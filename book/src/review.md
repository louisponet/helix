## Inline review

`:review` compares the current buffer with the file's content staged in the git
index, so only unstaged changes are shown. `:review <revision>` instead uses a
commit snapshot; `:review-commits` opens a picker of recent commit baselines.
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
gutter and the change motions (`]g`, `[g`, `]G`, `[G`) compare against the
review baseline instead of the git index, so they follow the review even after
its changes are committed. Closing the review, `:review-baseline clear` or
hitting the [size limits](#size-limits) restores the ordinary diff.

Old lines are shown directly above the lines that replaced them. Lines removed
from the very start of the file cannot be shown above the first line, so they
are shown below the lines that replaced them (or below the first line) under a
label such as `old lines 1-2 removed before line 1:`.

| Command | Description |
| --- | --- |
| `:review` | Start or switch review to the git index. Fails for files that are not in the index or have unmerged (conflicted) entries. |
| `:review <revision>` | Start or switch review to a commit, such as `HEAD~1`, a branch, tag or commit hash. |
| `:review-commits` | Choose a baseline by hash or subject from the latest 200 commits reachable from `HEAD`. |
| `:review-old` | Focus the old text nearest the cursor. |
| `:review-refresh` | Reload the buffer from disk; reread the index in index mode, or retain the selected commit in commit mode. Refuses unsaved changes. |
| `:review-comment <text>` | Add a pending comment on the primary selection, or on the selected old text while it is focused. Use `%%` for a literal `%`. |
| `:review-comments` | Pick a pending comment by file, lines or text and jump to its line in the current text. |
| `:review-uncomment [count]` | Drop the latest pending comment, or the `count` latest ones, and show how many remain. |
| `:review-export <path>` | Write all pending comments and reviewed files to a new JSON file. |
| `:review-close` | Stop reviewing the buffer. Pending comments are kept. |
| `:review-baseline <revision>` | Review all unmodified workspace buffers, and buffers opened later, against a commit. `clear` closes all reviews. |
| `:review-handoff <dir>` | Export a batch to `<dir>` on every write of a reviewed buffer. `clear` stops it. |
| `:review-send` | Export the handoff batch now without writing any file, for example to acknowledge a review without edits. |
| `:review-files` | Pick a file added (`A`), modified (`M`) or deleted (`D`) since the review baseline. |

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
Use `:review` without arguments to return to unstaged-only review.

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
  `:review-export`, `:review-send`, `:review-close` and `:review-old`.
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
   locations and reopened before exporting; renames are not tracked and pending
   entries cannot currently be removed individually.
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
for the editor. Every unmodified buffer in the workspace is reloaded from disk
and reviewed against it; buffers with unsaved changes are skipped and picked up
on their next write, and buffers opened or reloaded later are reviewed
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
