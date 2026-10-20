# Fork workflow

- `origin` is `helix-editor/helix`. `fork` is `louisponet/helix`.
- Keep `master` as a clean mirror of upstream `master`.
- `daily-driver` is the fork's default branch and the base for our installed HX.
- Keep the main checkout on `daily-driver`. Use separate worktrees for features
  and fixes, starting from `fork/daily-driver`.
- Target pull requests at `louisponet/helix:daily-driver`. Merge reviewed and
  tested changes with merge commits to preserve feature history.
- Test upstream updates on a topic branch before merging them into `daily-driver`.
- Follow [the fork workflow](docs/fork-workflow.md) for commands and validation.
