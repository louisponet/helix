# pi-helix-review

A [pi](https://github.com/earendil-works/pi) extension that imports a Helix
inline-review batch into the pi session you choose. It adds one command:

```
/helix-review <batch-file>
```

The batch format is described in [`schema.md`](schema.md) (version 2). Helix
never notifies pi: nothing is watched, polled, or sent until you run the
command yourself.

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
   / `:review-commits` (against a commit), edit or comment, and save.
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

## Tests

Requires Node 23.6 or newer (TypeScript type stripping) and `git`; no install
step:

```bash
cd contrib/pi-helix-review && npm test   # or: node --test helix-review.test.ts
```

The tests drive the registered `/helix-review` command against temporary Git
repositories and batch files, with a recording stand-in for pi's message API.

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
