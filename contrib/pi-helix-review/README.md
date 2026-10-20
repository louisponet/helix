# pi-helix-review

A [pi](https://github.com/earendil-works/pi) extension that imports a Helix
inline-review batch into the pi session you choose. It adds one command:

```
/helix-review <batch-file>
```

The batch format is described in [`schema.md`](schema.md) (version 2). With
the command alone Helix never notifies pi; [hook mode](#hook-mode) lets a saved
review in Helix hand itself back to the agent.

## Setup

Load the extension for one pi run:

```bash
pi -e /abs/path/to/helix/contrib/pi-helix-review/index.ts
```

The extension has no runtime dependencies. `package.json` is also a pi package
manifest (`pi.extensions`), so the directory can be installed as a local pi
package; that path is not exercised by the tests.

## Workflow

1. Pause pi (and any agents it started) so the working tree stops changing.
2. In Helix, `:review` each file (against the Git index), or `:review <revision>`
   / `:review-commits` (against a commit), edit and save.
3. `:review-export /tmp/review.json`.
4. In the idle pi session that should continue: `/helix-review /tmp/review.json`.

A relative path resolves against pi's working directory; `~/` is expanded.

## What the command does

- Refuses to run while pi is streaming, has queued steering/follow-up
  messages, or another `/helix-review` import is running, and checks again after reading the batch and the repository. If pi
  became busy, or the session was switched, replaced, or reloaded meanwhile,
  nothing is sent.
- Validates the batch: UTF-8 JSON of at most 4 MiB, `format`
  `"helix-review-batch"`, `version` `2`, a `git-index` or `git-commit` baseline
  with full hex object ids on every file and comment, and the field types of
  every file and comment (at most 1000 of each). Unknown extra fields are
  ignored. An invalid batch is rejected with the reason; nothing is sent. A
  version 1 batch from an older Helix is rejected too: re-export the review
  with an updated Helix.
- Sends exactly one ordinary user message, which starts a normal agent turn. It
  never steers or queues. The message lists the reviewed files (including files
  without comments), quotes each comment's selected text, surrounding context,
  and reviewer comment as fenced review data, and tells the agent to re-read the
  saved files before editing and to locate comments by text, using line numbers
  only as hints. Every file and comment is labelled with its own baseline (index
  blob, or commit and blob, or a path absent from the commit), and old text from
  a commit is marked as historical.
- Reports the handoff as submitted, not delivered: pi starts the turn after the
  command returns. If that fails (for example no model is selected or its
  authentication expired), pi shows an extension error for
  `send_user_message` and the handoff is not in the session; fix the cause and
  run the command again.
- Checks for stale data and lists it under a warnings heading instead of
  remapping anything onto new lines:
  - a reviewed file that is missing, unreadable, or not a regular file;
  - for an index baseline: a file whose Git index entry changed since export,
    is gone, or is unmerged, or that could not be checked (for example outside
    a repository); an old-side comment whose index baseline is no longer the
    index entry;
  - for a commit baseline: a commit that is unavailable or not a commit, or
    whose entry at the path is not the exported blob (or exists although the
    batch says the path is absent there). A commit is never compared with the
    current index or `HEAD`, so later commits or staging do not make it stale;
  - a current-side comment whose selected text and context are not found
    together in the saved file read as UTF-8 (it reports where the text alone
    appears, if anywhere). This can mean the file was edited, or that it is
    saved in another encoding;
  - a comment for a file the batch does not list.

  When a current-side selection is intact but has moved, the message gives its
  new line next to the comment-time hint.

The batch and the reviewed files are only read. Index entries are read with
`git ls-files --stage` and commit entries with `git ls-tree <commit>^{commit}`,
run directly (no shell) with literal pathspecs, validated full object ids, and
`core.fsmonitor` set to an empty value, which disables it on every Git version,
so no repository hook, filter, or driver runs. Blob contents are not read. A
symlink is resolved first, as Helix does, so its target's entry is compared.

## Hook mode

When an interactive pi TUI starts in a Git repository, the extension keeps a
running Helix in sync with the agent over its command socket (see "Driving Helix from
pi" in the Helix book). The socket is `$HELIX_COMMAND_SOCKET`, or
`$XDG_RUNTIME_DIR/helix/<id>.sock` (`/tmp/helix-<uid>/<id>.sock` without
`XDG_RUNTIME_DIR`), where `<id>` is the first 32 hex digits of the SHA-256 of
the canonical `git rev-parse --show-toplevel`. Helix not running is normal:
nothing is reported.

The mailbox is `<repo>/.pi/helix-review/` with an `outbox/` directory and
a `state/<session id>.json` file per pi session (its review baseline, the
per-file turn snapshots, when each file was last changed in one of its turns,
its pid while it runs, `last_active` and `cwd`); a `state.json` of an earlier
version moves into the first session's file. Ignore `.pi/` in Git.

- The review always compares against `HEAD`: commits of any kind (the
  agent's, yours in a shell or lazygit, a rebase) move it, except a baseline
  you pinned in Helix. pi checks `HEAD` at the start of each turn, on every
  refresh and on each 2 s socket check, so a commit made outside pi reaches
  Helix within 2 s as `review-baseline <HEAD>`; the committed hunks drop out
  of the review. Each move is kept as the session's `commit_floor`. After a
  `:review-commit` handoff, the end of the turn it starts (or steers) sends
  `review-baseline <commit> force` once the agent committed, which also
  releases a baseline you pinned in Helix, and echoes `pi-1: Agent turn done,
  committed <oid>, N files still changed`; if the agent did not commit,
  nothing moves. A `:review-comment` (or `:review-ask`) handoff ends with an
  instruction to do what the question asks, listing each selection with its
  lines, selected text and message. Only commented files and the overlapping
  or closest diff hunks are included, with the selection's three-line context;
  unrelated files, distant user edits and full enclosing functions are omitted.
  Explicit message and commit handoffs keep their broader review context; you read the answer in
  pi, and the turn ends with the ordinary echo. pi sends `review-handoff <outbox>` and `review-baseline <commit>`,
  so Helix reviews every open file in the workspace against that commit and
  writes a batch into the outbox when you hand back explicitly
  (`:review-comment`, `:review-send`, `:review-commit`, `:review-commit-all`;
  saving hands nothing back). It then
  sends `echo pi-1: Agent turn started` to Helix's status line (every echo
  starts with the session's label, see below).
- A new pi session (or `/reload`) attaches to Helix: it sends
  `review-handoff <outbox>` (even in a repository without a state file yet),
  restores `review-baseline <commit>` from the state when there is one,
  and echoes `pi hook attached as pi-1` (with `, baseline <commit>` when one was
  restored), so the status line shows the link is live. pi also checks the
  socket every 2 s, silently: when it appears, or is a new file (its inode and
  change time differ from the one attached to, as after a Helix restart), pi
  attaches the same way, so a Helix started after pi, or restarted, is picked
  up within 2 s without a new turn or a failed send. A send that finds no
  Helix listening also makes the next check attach again.
- After `edit`, `write` and `bash` tool results (coalesced within 200 ms) and
  at the end of the turn pi sends `review-baseline <commit>` again, which
  refreshes Helix's review.
- At the end of the turn pi stores a snapshot (`git hash-object -w`) of every
  file changed since the baseline, including untracked files but not pi's own
  state under `<repo>/.pi/`, and then echoes a summary such as `pi-1: Agent turn
  done, 3 files changed since 1a832de9 (:review-files)` to Helix's status line. A failing echo is ignored like
  every other socket command.
- A batch none of whose files is in the repository (after `realpath`) is
  meant for another agent: pi logs one line and deletes it, without a turn or a
  receipt.
- Each batch in the outbox becomes one `helix-review:handoff` message (entries
  under `<repo>/.pi/` are skipped): your edits as unified-diff hunks against the turn snapshot (or the batch's
  baseline blob when there is none; at most 200 hunk lines per file, the rest
  summarized with `truncated: true`) and the review comments. It starts a turn when pi is idle and steers the running turn
  otherwise. A batch can also carry a request from `:review-commit [subject]`
  (commit exactly what is staged, touching nothing else, and report the hash)
  or `:review-send <text>` (a free-text instruction); the message then ends
  with a "User request" section, and such a batch is sent even without edits
  or comments. The batch is then deleted. A batch with no edits, comments or
  request is dropped without a message. Either way pi echoes a receipt to Helix's status line, such as
  `pi-1 received your review: 2 files with edits, 1 review comment` or `pi-1 received your review: nothing to do`. Invalid batches stay
  in the outbox untouched.
- While the mailbox exists, the system prompt explains the handoff: you hand
  back explicit requests (review comments on selections, a message, a commit
  request), possibly mid-turn as a steering message; integrate your edits
  since the agent's turn snapshot and never revert them, keeping parts of the
  agent's change you undid undone (with tests and docs adjusted); report per
  file what was integrated or declined; and never commit or push on its own, even
  when other instructions say to, but only commit exactly the index when a
  `:review-commit` (commit-staged) handoff asks for it, so your partial
  staging in Helix is not swallowed.
- Ownership: `state/ownership.json` (shared by the repository's sessions,
  written atomically) records who changed which uncommitted lines:
  `{ "files": { "<abs path>": { "blob": <oid the ranges refer to>, "ranges":
  [{ "start", "end" (exclusive, 1-based; equal for lines deleted before
  `start`), "owner": "pi-2" | "user", "agent": the last agent owner, kept when
  you overwrite its lines, "at": ms }] } } }`. Each change is diffed against
  the recorded blob: the changed lines go to the writer (last writer wins),
  other ranges shift. The writer is the session after its `edit`/`write`
  (that file) or `bash` (every changed file) tool results and at the end of
  its turn; `user` when the 2 s check finds a change while no live session is
  in a turn (`in_turn` in the state files). A file seen for the first time
  gives what already differed from `HEAD` to `user`. Ranges a commit took in
  are dropped, so the ledger only holds uncommitted work. Only readable text
  files inside the repository (outside `.pi/`, at most 1 MiB) are attributed;
  other entries are dropped when the ledger is read, and a file whose update
  fails is skipped with one warning per session. The 2 s check only runs when
  `git status` or a changed file's size or time moved.
- Routing by ownership: a batch without a `target` goes to the sessions owning
  the hunks its comments are on (the range's agent, for your own edits the
  agent you overwrote, else the session that last changed the file, else the
  least busy one: the longest idle of the sessions not in a turn (oldest
  `last_active`), or when all are in a turn the one whose turn started first
  (`turn_started`)). When that is several sessions, the first session
  to see it rewrites it as `<name>.<label>.json`, one per owner, each with only
  its comments (and the batch's files) and targeted at it, so each owner claims
  its own. A `target` still wins.
- Commit requests by ownership: `:review-commit` / `:review-commit-all` go to
  the owner of the first staged hunk (`git diff --cached`, attributed through
  the ledger). When the index also holds other sessions' hunks, that owner gets
  `<mailbox>/commits/<batch>-<label>.patch` with only its hunks (listed as
  `path:lines`) and is told to commit them through a temporary index
  (`GIT_INDEX_FILE=<tmp> git read-tree HEAD`, `git apply --cached`,
  `git commit`), leaving the real index alone. At the end of that turn, once
  `HEAD` moved, the owner of the first remaining staged hunk gets the next
  request the same way; the last one commits the real index as usual. Hunks
  of a session that is gone go to the least busy one, chosen as above.
- Worktrees: a session started in the main checkout that edits files in a
  `git worktree` of it (or changes one with `bash`, noticed at the end of the
  turn from `git worktree list` and `git status`) gets a second hook for that
  worktree: its own mailbox under `<worktree>/.pi/helix-review`, state file,
  label and Helix socket, with the worktree's `HEAD` as its first baseline. The
  Helix running in the worktree is attached and refreshed like the main one,
  reviews saved there reach the session, and turn and receipt echoes go to the
  Helix of the worktree they concern (a worktree's end-of-turn echo only when
  it has changes). These hooks last for the session.

### Multiple sessions

Several interactive pi TUI sessions can be hooked to one repository. Subagents
and RPC, JSON and print-mode sessions do not attach or receive handoffs, even
when they edited the hunk. Routing falls back to an eligible TUI session. A
session is eligible while its pid runs and its state file records `tui: true`;
older sessions need `/reload` to register that capability. A target naming an
ineligible session stays pending instead of being delivered elsewhere. Each
session is labelled `pi-N`, the smallest number no other live session in the
repository uses, kept in its state file
and shown in its echoes (`pi-2: Agent turn started`, `pi-2 received your
review: ...`, `pi hook attached as pi-2`) and in pi's footer (`hx:pi-2`, `hx:pi-2+1`
with one worktree hook, `hx:pi-2 (no helix)` while no Helix answers). The baseline sent to Helix is the
newest commit floor of the live sessions (the latest `HEAD` any of them saw),
else the session's own baseline.

The commit floors of the live sessions are checked against the current
history before they are used: after the branch was rebased onto a newer base,
a floor that was the branch's old base moves to the new merge base, and one
the rebase rewrote moves to the branch commit with the same `git patch-id`
(or to the merge base when the rebase dropped or squashed it). The base branch
is the pull request's base (`gh pr view`, once per session), else
`origin/HEAD`, else the first of `origin/develop`, `origin/main`,
`origin/master`, `develop`, `main`, `master`. It logs `helix-review: baseline
<old> rewritten as <new> by a rebase`.

Each batch goes to one live session: the one
its `target` names (a session id or label such as `pi-2`), else the one that last changed one of its
files during a turn, else the least busy one (as for routing by ownership). Only that session
claims it, by renaming it to `<name>.<session id>.claimed` (if the rename
fails another session got it first) before handling and deleting it. On
start, a session puts back claims older than 10 minutes whose session is no
longer live, and deletes the state files of sessions whose pid is not running.

The JSON in the handoff message is described in [`schema.md`](schema.md).

## Tests

Requires Node 23.6 or newer (TypeScript type stripping) and `git`; no install
step:

```bash
cd contrib/pi-helix-review && npm test   # or: node --test helix-review.test.ts
```

The tests drive the registered `/helix-review` command and the hook-mode
events against temporary Git repositories and batch files, with a recording
stand-in for pi's event and message API and a fake Helix command socket.

## Limitations

- Old-side text is not compared with the baseline blob: the batch holds the
  text after Helix's worktree conversions, while Git holds the raw blob. The
  object id comparison is what detects a changed or mismatching baseline.
- There is no content hash of the saved files in the batch, so edits made after
  export are only noticed through current-side comments whose text no longer
  matches. The agent is told to re-read every file either way.
- Saved files are read as UTF-8, so current-side comments in a file saved in
  another encoding are reported as not found. Files over 8 MiB are not checked.
- If the session is replaced during the import, the old command context can no
  longer show a notification, so the refusal is raised as an extension error.
