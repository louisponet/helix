# Maintain our Helix fork

Use `daily-driver` as the base for our HX builds and new features. It includes
the inline review and T3 integration work. Keep the fork's `master` as a clean
mirror of `helix-editor/helix:master`.

In the main checkout, `origin` points to upstream Helix and `fork` points to
`louisponet/helix`. Keep this checkout on `daily-driver`: the office HX launcher
loads its bridge sources and runtime from here.

## Start a feature

From the main checkout, create a separate worktree so experiments leave the
daily editor's sources alone:

```sh
git fetch fork
git worktree add --no-track -b feat/my-change ../helix-my-change fork/daily-driver
cd ../helix-my-change
```

Implement the change, run the relevant checks from
[the contributing guide](CONTRIBUTING.md#testing), and try the changed editor
workflow. For shared review or T3 bridge changes, also run:

```sh
node --test contrib/t3-helix-review/*.test.ts contrib/helix-review/*.test.ts
```

Commit the change, then push the feature branch and open a PR against `daily-driver`:

```sh
git push -u fork feat/my-change
gh pr create --repo louisponet/helix --base daily-driver --head feat/my-change
```

After review and successful checks, merge the PR with a merge commit. The Build
workflow runs on PRs and on pushes to `daily-driver`.

## Bring in upstream changes

From the main checkout, fetch upstream and advance the fork's mirror:

```sh
git fetch origin master
git push fork origin/master:refs/heads/master
```

If Git rejects this push, inspect the divergence before changing the mirror.

Integrate upstream in a separate worktree:

```sh
git fetch fork
git worktree add --no-track -b chore/update-upstream ../helix-update-upstream fork/daily-driver
cd ../helix-update-upstream
git merge --no-ff origin/master -m "chore: merge upstream Helix"
```

Resolve conflicts, run the checks, and trial HX. Push this branch and open a PR
against `daily-driver`, following the same review process as a feature.

## Update the daily checkout

After a merge, update the main checkout:

```sh
cd ~/Software/helix-editor
git pull --ff-only
```

Build and install HX from this branch when promoting a new version. A GitHub
merge does not rebuild the installed binary or update Zoomyboi's bundled bridge.
