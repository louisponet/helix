# Shared Helix review protocol

This module supplies the provider-independent parts of the Pi and T3 Code
adapters. It uses Node built-ins and Git, with no runtime packages. Keep this
directory beside the adapter directories when loading or copying them; an
adapter's directory alone is not a standalone distribution.

```ts
import { ReviewBatch, buildHandoff } from "../helix-review/index.ts";

const batch = await ReviewBatch.read(batchPath);
const states = await batch.inspect();
const { warnings } = batch.render(batchPath, states);
const handoff = await buildHandoff(batch, {
  repo,
  snapshots, // absolute path -> raw Git blob from the recipient's last turn
  baseline,  // optional commit id for the recipient's review baseline
  strictCurrentReads: true, // keep unavailable existing content pending
  excludePath: (path) => path.startsWith(stateDirectory + "/"),
});
// Surface warnings and deliver handoff.content/handoff.details using the adapter.
```

`ReviewBatch.read` checks UTF-8, the 4 MiB input limit and the version 2 schema.
`ReviewBatch.parse` parses an already decoded JSON string. Both reject malformed
metadata with `BatchError`; unknown fields are ignored. Parsed properties use
camel case, while handoff details use the existing wire schema's snake case.
The batch schema remains documented in
[the Pi adapter's schema](../pi-helix-review/schema.md).

`inspect` reads current saved files and checks raw Git index/commit entries.
`render` creates a readable batch handoff and reports missing files, changed
index entries, inconsistent commit metadata and current selections that cannot
be located with their original context. Immutable commits are checked against
the specified commit, independently of the current HEAD or index. Quoted old
text is authoritative because Helix's worktree conversions can differ from
raw Git blobs. Inspection never runs repository hooks or filters.

`buildHandoff` constructs the existing `helix-review-handoff` v1 payload and
request instructions. It diffs saved files against recipient turn snapshots,
falling back to each file's exported baseline. An `ask` includes only commented
files, the nearest/overlapping baseline diff hunks for each selection and the
corresponding user edits. Current selections are located by their quoted text
and context before consulting comment-time line hints. Full enclosing syntax
nodes and unrelated changes are omitted for asks. Diffs explicitly use three context lines and no inter-hunk context,
independent of repository diff settings. A `pr-review` diffs each file against
`HEAD` instead (`against: "head"`) without context lines, so every hunk maps
onto the pull request's head lines. Whole hunks are capped at
200 lines; oversized remaining hunks are summarized with `truncated: true`.

Per-comment baselines are retained in new handoffs. Pi passes
`includeCommentBaselines: false` to preserve its existing payload. A supplied
`commitPatch: { patch, hunks }` replaces the ordinary staged-commit request
with instructions to commit that patch through a temporary index. A batch
with neither edits, comments nor a request produces `undefined`.

The builder may store raw content in Git's object database. It does not change
working files, stage content, or create commits. It propagates Git failures,
so adapters must retain batches that could not be built. The builder does not
perform `inspect` itself; adapters decide how to surface validation warnings.
With `strictCurrentReads: true`, unavailable existing files (including oversized
or non-regular files) abort construction, even if an earlier inspection succeeded.
Missing paths still represent deletions. Without this option the original Pi
behavior treats unreadable content as empty text, so inspection warnings matter. Routing, claims, durable delivery, provider
lifecycle and transport belong to the adapters.

Run the shared tests with Node 23.6 or newer:

```sh
node --test contrib/helix-review/helix-review.test.ts
```
