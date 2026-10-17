# Work together and review PRs in HX

Review an agent's changes, ask about selected code (including deleted lines),
save corrections, and hand the work back to the same T3 thread. Replies appear in
T3. **Work together** edits the checkout and sends feedback to the agent.
**Review a PR** holds comments and code suggestions as drafts for GitHub, leaving
checkout files unchanged. Each mode keeps its own editor session and buffers.

## Start

Requires this Helix build with review commands and its command socket, Node
23.6 or newer, and T3 Code with HTTP MCP enabled. Run on the workspace host
(Linux/macOS, or inside WSL). Keep this directory and its sibling
`../helix-review` in the checkout; the launcher runs directly from source.

```sh
cargo build -p helix-term
node contrib/t3-helix-review/cli.ts install
hx-t3 setup
hx-t3
```

Ensure `hx` points to the build and `~/.local/bin` is on your terminal's PATH.
Use `--hx /absolute/path/to/hx` to select another executable.

`hx-t3` picks among threads using the current checkout. To choose explicitly,
copy a thread's link from T3 and run `hx-t3 --thread <link-or-id>`.
The patched T3 frontend also exposes **Work together in HX** in the thread's workspace
details and command palette. It opens a dedicated terminal in that thread's
environment. On Android and iOS, choose **Work together in HX** from the thread's
**Terminal** menu. The installed T3 release needs the patched frontend to show this
button; the terminal command works independently.

## Work together

1. Open HX before the next agent turn, or pass `--base main` (or a commit) to
   review work already committed. First use otherwise starts at `HEAD`.
2. Use `Space A f` to pick a changed file and `Space A b` to inspect another
   baseline. The bridge retains its opening baseline across agent commits and
   restarts; use `--base` to change the retained range.
3. Select code and press `Space A a` to ask a question. `:review-old` focuses
   deleted text; `Esc` returns to the working file.
4. Edit and save corrections normally. Saving alone sends no agent message.
   Use `:review-send <instruction>` when ready to hand the saved work back.
5. Stage selected uncommitted hunks with `Enter`, then `:review-commit [subject]` to ask
   the agent to commit the staged content. It waits for an active turn to
   finish and requires the same HEAD and staged tree before committing.

Quit HX to stop the bridge. Reopen the same thread to resume retained feedback.
In T3, choosing the action again focuses the running editor. After you quit HX,
the action opens a fresh editor.

In T3, **Comment** saves feedback on the current selection without waking the
agent. **Undo comment** removes the latest pending comment. **Send to agent** delivers the
pending feedback; **Ask agent** immediately asks about the selection. Save corrections
before sending. Replies appear in the same T3 thread.

Opening a source file also works outside Git or before the first commit. **Files**
opens the ordinary file picker there; comparisons and agent feedback become
available after the checkout has a commit and the editor is reopened.

## Review a pull request

Open a PR's code in T3 or select **Review a PR** in a thread with a linked PR.
The launcher verifies that the checkout matches the PR head without switching
branches. `hx-t3 --mode review` uses the linked PR;
`hx-t3 --pr <GitHub PR URL>` selects it explicitly. Merely linking a PR never
changes Work together into review mode.

Use **Draft PR comment** for feedback on selected lines, including deleted text.
Edit the code in HX to create suggestion drafts. The buffers start from the
PR's exact head and cannot be written over source files, even with `:write!`.
Inspect the original and proposed code in the review panel and include each
suggestion you want to publish. **Ask agent** requests analysis or replacement
code in the conversation without asking it to modify the checkout.
**Mark reviewed** tracks your progress independently.

In **PR Review**, edit or remove comments, choose Comment, Approve, or Request
changes, and preview the combined review before submitting. Only included
suggestions are posted, against the PR commit you reviewed. GitHub edits that
cannot be expressed as inline suggestions show a reason and cannot be included.
Existing thread replies must be posted separately on the PR.

Draft comments and suggestion buffers remain in the running HX session through
mode switches and phone reconnects; publish before quitting HX. Changing an edit
clears its inclusion so you can inspect it again. Submission preserves checkout
files, the Git index, and your branch. If submission cannot be confirmed, check the PR before trying again;
the app keeps the drafts and does not retry the GitHub write automatically.

## Delivery and review boundaries

Each editor binds to one server, thread, worktree, branch, and mode. Moving the thread
or switching branches stops delivery and retains pending feedback. The bridge
uses a private socket and mailbox, separate from Pi. An explicit `@target` for
another session is refused.

Handoffs are frozen before sending and retried with the same T3 request ID.
Accepted receipts survive restarts. A connection failure may mean T3 already
accepted the message; reopening retries safely with that ID.

In Work together, the bridge observes saved files when it sees a completed agent turn. These are
filesystem observations, not exact T3 checkpoints: simultaneous user and agent
edits cannot always be separated into distinct hunks. Explicit handoffs still
tell the agent to re-read and preserve your saved files. Keep unsaved edits in
HX until ready to save; freshness warnings accompany changed selections.

Staged commit requests freeze HEAD and the Git index. If you change either
before delivery, the request stays pending instead of committing new content.
No push is requested. To cancel held feedback, quit HX, run `hx-t3 pending`
in its worktree, then `hx-t3 discard --batch <name>`. This keeps an archive of
the original batch. Reopen HX to retry remaining feedback or make a new request.

## Connection and troubleshooting

Local setup uses T3's official CLI and OAuth pairing and saves a private MCP
login. Run `hx-t3 setup` when it expires; revoke **Helix review** in T3
Settings → Connections. `--base-dir` selects another T3 home; `--t3` selects
its CLI. Never point a development server at your live T3 state.

The native button checks the destination environment's identity before loading
credentials or reading a thread. It keeps a separate login for each environment.
Conflicting explicit MCP credentials produce an error instead of redirecting
your feedback to another server.

For a remote workspace, run the launcher on that host. Explicit
`HX_T3_MCP_URL` and `HX_T3_MCP_TOKEN` can supply an existing MCP connection.
Keep tokens out of shell history and checked-in configuration.

Use `hx-t3 --help` for socket, state-directory, and existing-editor attachment
options. Private delivery state lives under
`${XDG_STATE_HOME:-~/.local/state}/helix/t3-review`.
After a crashed bridge, tiny `bridge.lock.stale-*` directories may remain.
They allow safe concurrent recovery and do not prevent reopening. An incomplete
lock reports its exact path; close all editors for that thread before removing
that lock and reopening.

Focused checks:

```sh
node --test contrib/t3-helix-review/*.test.ts contrib/helix-review/*.test.ts
```
