# Helix review for T3 Code

`hx-t3` binds a Helix editor to one T3 thread. Questions, comments, saved
edits and pull request reviews you send from HX reach that thread's agent, and
the answers appear in the thread. T3 can start HX for you, or you can run
`hx-t3` in a terminal.

## Set up

Requires this Helix build with review commands and its command socket, Node
23.6 or newer, and T3 Code with HTTP MCP enabled. Run on the workspace host
(Linux/macOS, or inside WSL). Keep this directory and its sibling
`../helix-review` in the checkout; the bridge runs directly from source.

```sh
cargo build -p helix-term
node contrib/t3-helix-review/cli.ts install
```

Make sure `hx` points to the build and `~/.local/bin`, where `hx-t3` is
installed, is on the PATH of the T3 server and your terminals. Then run this
once in a T3 terminal:

```sh
hx-t3 setup
```

Setup saves a private login for that T3 environment. Without it, Helix in T3
still opens files and diffs, but feedback is not delivered, and HX's status
line tells you to run setup.

## Helix in T3

With **Use Helix** on, T3 starts one HX per checkout (worktree or project
root) the first time you open code there. The editor keeps running when you
close the panel or switch threads, and every window and device shows the same
editor. Clicking a file in the diff panel opens its diff in HX.

Next to the editor, T3 runs the bridge for the thread you are viewing. When
another thread opens the same checkout, the bridge switches to that thread.
HX's status line shows the bound thread as `T3: <title>`.

In HX:

- `Space A a` asks the agent about the selection right away, including
  deleted text focused with `:review-old`. Pending comments stay pending.
- `Space A c` adds a pending comment on the selection. `Space A C` lists
  pending comments and `Space A u` drops the latest one.
- `Space A S` (`:review-send [instruction]`) sends the pending comments and
  your saved edits to the agent.
- `Space A P` (`:review-send-pr [summary]`) asks the agent to post them on the
  thread's pull request: each comment as an inline review comment, each saved
  edit since `HEAD` (also in files you closed) as a suggestion on the lines it
  replaces, and the summary as the review body.
  The agent publishes with T3's pull request review tool and leaves your edits
  uncommitted in the checkout.
- `Space A m` (`:review-commit [subject]`) asks the agent to commit exactly
  what you staged with `Enter`.

Saving alone sends nothing. Save corrections before sending.

Before `Space A P`, link the pull request to the thread and push your branch.
With several linked pull requests, the agent uses the one whose head branch is
the checkout's branch. With none, the review stays pending and HX shows
`Link a pull request to this thread first`; link one and the bridge sends it.
The bridge gives the agent your local `HEAD`, and T3's review tool refuses to
post when the pull request's head is a different commit. A review with no
comments, no changes since `HEAD` and no summary is not sent.

T3 no longer shows **Work together in HX** or **Review a PR**. HX's keys
replace the Helix toolbar and comment form.

## Terminal

`hx-t3` in a checkout opens HX bound to a thread that uses it, and asks which
one when several do. To choose explicitly, copy a thread's link from T3 and run
`hx-t3 --thread <link-or-id>`. The same keys work as in T3. Quit HX to stop the
bridge; opening the thread again resumes retained feedback.

- First use reviews from `HEAD`. Pass `--base main` (or a commit) to review
  work already committed. The bridge keeps that baseline across agent commits
  and restarts. If an amend, rebase or force-push drops it from the branch,
  the running bridge reviews from the new `HEAD` instead.
- `Space A f` picks a changed file and `Space A b` inspects another baseline.
- `hx-t3 --mode review` (or `--pr <url>`) opens a separate editor whose code
  buffers are suggestion drafts from the pull request's head, leaving checkout
  files unchanged. `Space A a` asks the agent about them. To publish a review
  from T3, use the default mode and `Space A P` instead.

Opening a source file also works outside Git or before the first commit.
Comparisons and feedback become available after the checkout has a commit.

## Delivery and review boundaries

Each bridge binds to one server, thread, worktree, branch and mode. Moving the
thread or switching branches stops delivery and keeps pending feedback. The
bridge uses a private mailbox, separate from Pi. An explicit `@target` for
another session is refused.

Handoffs are frozen before sending and retried with the same T3 request ID.
Accepted receipts survive restarts. A connection failure may mean T3 already
accepted the message; the next attempt retries safely with that ID.

The bridge observes saved files when it sees a completed agent turn. These are
filesystem observations, not exact T3 checkpoints: simultaneous user and agent
edits cannot always be separated into distinct hunks. Explicit handoffs still
tell the agent to re-read and preserve your saved files. A pull request review
diffs your saved files against `HEAD`, so leave the agent's own uncommitted
work out of files you review that way.

Staged commit requests freeze HEAD and the Git index. If you change either
before delivery, the request stays pending instead of committing new content.
No push is requested. To cancel held feedback, run `hx-t3 pending` in the
worktree, then `hx-t3 discard --batch <name>`. This keeps an archive of the
original batch.

## Connection and troubleshooting

T3 runs `hx-t3 --attach --socket <path> --thread <id>` next to its editor.
This attached bridge never starts pairing. It exits when the editor closes.
When another bridge for the thread is still open, it waits up to 10 seconds
for that one to exit. When it cannot start (no saved login, an archived
thread, a branch mismatch, or another bridge still open for the thread), it
shows the reason in HX's status line and exits.

Setup uses T3's official CLI and OAuth pairing. Run `hx-t3 setup` in a T3
terminal again when the login expires, and revoke **Helix review** in T3
Settings → Connections. `--base-dir` selects another T3 home; `--t3` selects
its CLI. Never point a development server at your live T3 state. Each T3
environment keeps its own login, and conflicting explicit MCP credentials
produce an error instead of redirecting feedback to another server.

For a remote workspace, run the bridge on that host. Explicit
`HX_T3_MCP_URL` and `HX_T3_MCP_TOKEN` can supply an existing MCP connection.
Keep tokens out of shell history and checked-in configuration.

Use `hx-t3 --help` for socket, state-directory and attachment options.
Private delivery state lives under
`${XDG_STATE_HOME:-~/.local/state}/helix/t3-review`. After a crashed bridge,
small `bridge.lock.stale-*` directories may remain; they allow safe concurrent
recovery and do not prevent reopening. An incomplete lock reports its exact
path; close all editors for that thread before removing it.

Focused checks:

```sh
node --test contrib/t3-helix-review/*.test.ts contrib/helix-review/*.test.ts
```
