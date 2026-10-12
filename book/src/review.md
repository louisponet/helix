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
| `:review-comment <text>` | Send a comment to the agent at once: one comment per selection, or on the selected old text while it is focused, then hands the review off (writing the buffer first if it is modified). Needs a handoff directory (`:review-handoff` or the pi mailbox). A leading `@label` (`:review-comment @pi-2 rename this`) addresses the pi session with that label. While reviewing a pull request it adds a pending pull request comment on the primary selection instead. Use `%%` for a literal `%`. |
| `:review-ask <text>` | The same command as `:review-comment`. |
| `:review-comments` | Pick a pending comment, or while reviewing a pull request one of its comment threads, by kind, author, place or text, and jump to its line in the current text. |
| `:review-uncomment [count]` | Drop the latest pending comment, or the `count` latest ones, and show how many remain. |
| `:review-export <path>` | Write all pending comments and reviewed files to a new JSON file. |
| `:review-revert` | Replace the change under the cursor, or the focused old text's change, with the baseline's lines (added lines are deleted; removed lines shown below the cursor line are restored). One undoable edit; the file on disk is not touched. |
| `:review-stage` | Stage the unstaged hunk (index to worktree) under the cursor in the git index, like `git add -p`: in a reviewed buffer every hunk any selection touches (a selection spanning several hunks stages them all) or the hunk of the focused old text, in the `:review-diff` buffer the hunk under the cursor. Refuses buffers with unsaved changes, a locked index (`index.lock`), and files git converts (filters, line endings). |
| `:review-unstage` | Unstage the staged hunks the selections touch (same places as `:review-stage`): the index gets the review baseline's lines back for it. |
| `:review-toggle-stage` | Stage the hunks the selections touch, or unstage them all if every one is already staged, so hunks can be toggled in any order; hunks committed since the baseline are skipped and counted. All changes go into one index write. |
| `Enter` | In normal mode, runs `:review-toggle-stage` while the buffer is reviewed or is the `:review-diff` buffer, and does nothing otherwise. |
| `:review-close` | Stop reviewing the buffer. Pending comments are kept. Under a `:review-baseline`, the buffer is not reviewed again until a different baseline, `:review-baseline clear`, or an explicit `:review` on it. |
| `:review-baseline <revision>` | Review all unmodified workspace buffers, and buffers opened later, against a commit. Refuses a commit that is not `HEAD` or one of its ancestors (`commit <commit> is not in this branch's history`), such as a branch tip a rebase rewrote. `clear` closes all reviews. `:review-baseline <revision> force` adopts the commit even over a baseline pinned by you, ending the pin (the pi hook sends it after a commit you asked for). |
| `:review-pr <number\|url>` | Review a GitHub pull request: check it out with `gh pr checkout` and review every buffer against its merge base with the base branch. Refuses uncommitted changes and unsaved reviewed buffers. `close` leaves the pull request review and keeps the checkout. See [Reviewing pull requests](#reviewing-pull-requests). |
| `:review-submit [approve\|request-changes\|comment] [summary]` | Post the pending pull request comments as one review (see [Reviewing pull requests](#reviewing-pull-requests)). `G m` opens it while reviewing a pull request. |
| `:review-commit-all [subject]` | Stage every file changed since the baseline (modified, deleted and untracked, like `git add -A` limited to them), then hand back like `:review-commit`. Refuses unsaved reviewed buffers, and says `no changes since <commit>` when there is nothing. `G M` in the default keymap. |
| `:review-next` | Go to the next changed block that is unstaged or staged, skipping hunks committed since the baseline; after the last one, open the next file changed since the baseline with such a block (`:review-files` order, wrapping around) at its first one. Shows `<file>: hunk 2 of 5`, counting only those blocks, or `no uncommitted changes since <commit>` when there are none. `G n` in the default keymap, which takes a count. |
| `:review-prev` | Like `:review-next` backwards, landing on the previous file's last block. `G N` in the default keymap. |
| `:review-baseline-pick` | Pick the baseline from the first-parent history of `HEAD` (up to 300 commits: commit, age, author, subject), with `●` on the current baseline, `base` on the merge base with the base branch (the pull request's while reviewing one, else `origin/HEAD`, `develop`, `main` or `master`), `pi` on the baseline the pi hook last sent and `HEAD` marked; the current baseline is preselected. Choosing a commit works like `:review-baseline <commit>` and pins it as yours (`*`), except the `pi` row, which follows the hook again. `G b` in the default keymap; `G B` refreshes the buffer's review. |
| `:review-older` | Move the baseline to its first parent (pinning `HEAD` first when none is set) and refresh every review; the status shows `Baseline <commit> <subject> (N behind HEAD)`. `G -` in the default keymap. |
| `:review-newer` | Move the baseline to the next commit towards `HEAD` on its ancestry path and refresh every review. `G +` in the default keymap. |
| `:review-handoff <dir>` | Export the batches of the explicit handoffs (`:review-comment`, `:review-send`, `:review-commit`, `:review-commit-all`) to `<dir>`. Writing a buffer never hands anything off. Without it, the default is `<workspace>/.pi/helix-review/outbox` when that directory exists (the pi extension creates it) and every reviewed file and pending comment is in the workspace. `clear` returns to that default. |
| `:review-send [text]` | Export the handoff batch now without writing any file, for example to acknowledge a review without edits; optional text goes to the agent as an instruction (the batch's `request`). A leading `@label` (`:review-send @pi-2 [text]`) addresses one of several pi sessions (the batch's `target`). |
| `:review-commit [subject]` | Hand back like `:review-send`, asking the agent to commit exactly what is staged in the git index (optionally with that subject, and addressed to a session with a leading `@label`). Refuses when nothing is staged, naming how many of the buffer's hunks are already committed in `HEAD` if any. |
| `:review-diff` | Show one unified diff of every file changed since the review baseline (with none set, `HEAD` is pinned as it, like `:review-baseline HEAD`) in a read-only scratch buffer; running it again, or `:review-baseline` (as the pi hook does after each edit), refreshes that buffer in place. File headers are workspace-relative paths (`--- <path>`), so `gf` opens a file. |
| `:review-diff-open` | In the `:review-diff` buffer, open the file of the hunk under the cursor at the current line it shows (the next current line on a removed line). |
| `:review-files` | Pick a file added (`A`), modified (`M`) or deleted (`D`) since the review baseline, and open it at its first change. The `+/-` and `hunks` columns show added and removed lines and staged/total hunks plus the hunks already committed since the baseline (`?` over the size limits), and the status line the totals. The `agent` column names the pi session that last changed the file, from the hook's state files. With no baseline set, it first pins `HEAD` like `:review-baseline HEAD`. |

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
session. Saving never notifies pi: only the explicit handoff commands do.

1. Pause pi (and any agents it started) so the working tree stops changing.
2. In Helix, `:review` each file (`:review-refresh` picks up changes made since),
   edit and `:write`. (`:review-comment` needs a handoff directory, see
   below, and hands its comments off at once.) Wait for writing
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
   unstages it if it is already staged, in any order; with a selection over
   several hunks (or several selections) it stages every unstaged one, or
   unstages them all when all are staged (`Staged 3 hunks; 5 of 7 staged in
   file.rs`); after staging the cursor moves on to the next unstaged hunk, in
   this file or the next changed one (`; next: b.rs hunk 1 of 2`), and stays
   when none is left; after unstaging it stays on the hunk. On a line with nothing to stage, `Enter` (and `G s`) jumps to the
   next unstaged hunk instead (`next: b.rs hunk 2 of 5`, or `nothing unstaged
   since <commit>`), from a reviewed buffer,
   focused old text or the `:review-diff` buffer, where staged hunks are marked
   `(staged)`. In the buffer a staged hunk's lines turn blue and its old lines
   grey (`diff.plus.review.staged`, `diff.minus.review.staged`) until the next
   edit. Save first: only what is on disk is staged. A hunk `HEAD` already
   holds was committed after the baseline (by the agent, say): it gets its own
   colours (`diff.plus.review.committed`, `diff.minus.review.committed`, a faint
   tint in the default theme, falling back to the staged ones) and is counted as committed, and staging or unstaging it fails
   with `already committed in <commit>`. A block that mixes committed lines
   with newer edits next to them acts as one unit: `Enter` anywhere in it
   stages its uncommitted part (`Staged 1 hunk, 1 already committed`). While reviewing, `Enter` in
   normal mode is taken by this; elsewhere it does nothing.
7. `:review-comment <text>` (`G c` or `G a`) sends a comment on the current
   selections (several are fine), or on the selected old text while it is
   focused, to the agent at once, writing the buffer first if needed
   (`Sent 2 comments to the agent`). You watch the agent's edits arrive and
   read its answer in pi. A comment that could not be sent (another reviewed
   buffer has unsaved changes, say) stays pending and goes with the next
   handoff: `:review-comments` lists pending comments and jumps to one;
   `:review-uncomment [count]` drops the latest.
8. Your own edits are ordinary worktree changes: save them as usual (auto-save
   included), and stage them like any other hunk. Saving tells the agent
   nothing.
9. Hand the review back explicitly: `:review-send` (`G S`) exports your saved
   edits since the agent's turn and any pending comments;
   `:review-send <text>` adds an instruction for the agent. `:review-commit [subject]` hands back and asks
   the agent to commit exactly what you staged (and nothing else);
   `:review-commit-all [subject]` (`G M`) stages everything first. The status
   line then confirms it, for example `pi-1 received your review: 1 file with
   edits, 1 review comment`. With several pi sessions hooked to the repository,
   a handoff goes to the session that last changed its files (the `agent`
   column of `:review-files`); start the text with `@label` to pick one, for
   example `:review-send @pi-2 take this one`.

The baseline stays on the same commit until a handoff is received, so a
following agent turn is still reviewed against the commit you started from.
The first turn after a handoff moves the baseline to the then current `HEAD`.

`G b` (`:review-baseline-pick`) picks any commit of the branch history as the
baseline, with the current baseline, the merge base with the base branch, the
baseline the pi hook last sent (`pi`) and `HEAD` marked. `:review-older` and
`:review-newer` (`G -`, `G +`) step the baseline through history, for example
to see what the last few commits changed. A baseline you choose this way, or
with a typed `:review-baseline <commit>`, is pinned by you: the statusline's
baseline gets a trailing `*`, and the pi hook's `:review-baseline`, whatever
commit it sends, only refreshes the reviews against yours. Picking the `pi`
row in `G b` (or stepping onto the hook's baseline) ends the pin and follows
the hook again, as does `:review-baseline clear`, after which the hook's next
refresh pins its baseline. A commit you asked for (`G m`, `G M`) releases your
pin too: the hook then sends the new commit with `force`, and the review moves
on to it.

These commands are built into the default keymap under `G` in normal mode
(see [review mode](./keymap.md#review-mode)): `G f` picks a file, `G d` shows the
diff, `G o` focuses old text, `G c` and `G a` send a comment to the agent (`G C` lists pending ones), `G s` stages, `G m` asks the agent
to commit (`G M` stages everything first) and `G n` / `G N` move between changes not yet committed, on into the next or previous
changed file (`:review-files` order, wrapping around) at its edge. Keys you map under
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
`review` statusline element shows the baseline (with a trailing `*` when pinned
by you), the hunk count, how many hunks
are already committed since the baseline and how many are staged (each omitted
when zero, as in `9 hunks 3 committed 2 staged`), the pending comment count and
`@<label>` when the last handoff was addressed to a session; add it with, for example,
`right = ["review", "diagnostics", "selections", "position"]` under
`[editor.statusline]`.

### Reviewing pull requests

`:review-pr <number|url>` reviews a GitHub pull request with the review mode.
It talks to GitHub through the [`gh`](https://cli.github.com) CLI (`$HELIX_GH`
when set, else `gh` on `PATH`), in the background: it looks the pull request
up, runs `gh pr checkout` (which also works for forks), and pins the merge base
of the base branch and the checked-out head as the review baseline, so every
buffer, `:review-files`, `:review-diff` and `G n` show exactly the pull
request's changes. The status reads `Reviewing PR #123 <title>: N files
against <base branch> at <merge base>`, and the `review` statusline element
starts with `#123`. It refuses while the worktree has uncommitted changes.

The pull request's baseline stays pinned: the pi hook re-sending its own
baseline only refreshes the reviews. `G -` / `G +` still step through history
(`G +` from the merge base walks the pull request's commits). Running
`:review-baseline` yourself leaves the pull request review, and so does
`:review-pr close`, which keeps the checkout. After the branch was rebased onto
a newer base, `:review-refresh` (`G B`) moves the baseline to the new merge
base (`Baseline moved to <commit> after a rebase`). `:review-pr` also works over the
command socket, so other tools can open a pull request in Helix.

The pull request's existing review comments (`gh api .../pulls/<n>/comments`)
are shown as threads above the line they are on, greyed
(`ui.review.thread`): `login │ text`, replies indented, at most 12 rows each.
A comment on the base side is shown above the change that removed its line,
tagged `(old side)`; one on a line the pull request has since changed is
shown at its original line, tagged `(outdated)`. They reload with
`:review-refresh` (`G B`). `G C` lists them next to your pending comments.

While reviewing a pull request, `G c` (`:review-comment`) collects comments
for the pull request instead of for pi: they are kept out of the batches
handed to pi (`:review-send`, `:review-commit`), and a comment on a
line with a thread replies to it. `:review-ask` is the same command, so it does
likewise.

`:review-submit [approve|request-changes|comment] [summary]` (`G m` opens it
while reviewing a pull request) posts them as one review on the pull request's
head commit, with the summary as its body (`comment` by default). A comment on
lines of the pull request's diff (changed lines and 3 lines around them, on
their side) becomes a line comment, spanning lines when it covers several;
any other becomes a file-level comment quoting its lines (`> 11: text`).
Replies are posted to their threads. On success the pending comments are
cleared, the threads reloaded, and the status reads `Submitted APPROVE on #7:
2 comments, 1 reply, 1 file-level`; on failure everything stays pending and
the status shows `gh`'s error.

### Troubleshooting

- No `pi hook attached` status: Helix was not listening when pi started (it
  was started after pi, or in another workspace, which has its own socket).
  Open Helix in the repository and run `/reload` in pi (or restart pi); the
  next agent turn also attaches. Meanwhile `:review-files` reviews against
  `HEAD` on its own, but only pi sets the handoff directory, so the handoff
  commands have nowhere to export to until it attaches.
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

The handoff commands export a batch (the same version 2 JSON as
`:review-export`) to `<dir>/<unix-ms>-<pid>.json` of the handoff directory.
The file is first written under a `.json.tmp` name and then renamed. The batch
covers every unmodified reviewed buffer with its pending comments. Writing a
buffer, by `:w` or auto-save, never exports one.

`:review-send` exports the same batch immediately without writing any file, so
a review without edits can be handed back without touching the disk. It fails
when no handoff directory is set and there is no default outbox, and names the reviewed buffers that have
unsaved changes instead of exporting.
