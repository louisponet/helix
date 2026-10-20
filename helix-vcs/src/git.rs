use anyhow::{bail, Context, Result};
use arc_swap::ArcSwap;
use gix::filter::plumbing::driver::apply::Delay;
use gix::index::entry::{Flags, Stage};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use gix::bstr::{BString, ByteSlice};
use gix::diff::Rewrites;
use gix::dir::entry::Status;
use gix::objs::tree::EntryKind;
use gix::sec::trust::DefaultForLevel;
use gix::status::{
    index_worktree::Item,
    plumbing::index_as_worktree::{Change, EntryStatus},
    UntrackedFiles,
};
use gix::{Commit, ObjectId, Repository, ThreadSafeRepository};

use crate::{CommitBase, CommitEntry, CommitInfo, FileChange};

#[cfg(test)]
mod test;

#[inline]
fn get_repo_dir(file: &Path) -> Result<&Path> {
    file.parent().context("file has no parent directory")
}

pub fn get_diff_base(file: &Path, trust_full: bool) -> Result<Vec<u8>> {
    debug_assert!(!file.exists() || file.is_file());
    debug_assert!(file.is_absolute());
    let file = gix::path::realpath(file).context("resolve symlinks")?;

    // TODO cache repository lookup

    let repo_dir = get_repo_dir(&file)?;
    let repo = open_repo(repo_dir, trust_full)
        .context("failed to open git repo")?
        .to_thread_local();
    let head = repo.head_commit()?;
    let file_oid = find_file_in_commit(&repo, &head, &file)?.context("file is untracked")?;

    let file_object = repo.find_object(file_oid)?;
    let data = file_object.detach().data;
    convert_to_worktree(&repo, &file, data)
}

/// Returns the content of `file` staged in the git index (converted to its worktree form) and
/// the id of the index blob.
pub fn get_index_base(file: &Path, trust_full: bool) -> Result<(Vec<u8>, ObjectId)> {
    debug_assert!(file.is_absolute());
    let file = gix::path::realpath(file).context("resolve symlinks")?;

    let repo = open_repo(get_repo_dir(&file)?, trust_full)
        .context("failed to open git repo")?
        .to_thread_local();
    let work_dir = repo.workdir().context("repo has no worktree")?;
    let rela_path = gix::path::to_unix_separators_on_windows(gix::path::try_into_bstr(
        file.strip_prefix(work_dir)?,
    )?);
    let index = repo.index_or_empty()?;
    let entries = index
        .entry_range(rela_path.as_ref())
        .map_or(&[][..], |range| &index.entries()[range]);
    let entry = match entries {
        [] => bail!("{} is not in the git index", file.display()),
        [entry] if entry.stage() == Stage::Unconflicted => entry,
        _ => bail!("{} has unmerged entries in the git index", file.display()),
    };
    match entry.mode.to_tree_entry_mode().map(|mode| mode.kind()) {
        Some(EntryKind::Blob | EntryKind::BlobExecutable) => (),
        kind => bail!("index entry at {} is not a file: {kind:?}", file.display()),
    }
    // `git add --intent-to-add` stages the empty blob without writing it to the object database.
    let data = if entry.flags.contains(Flags::INTENT_TO_ADD) || entry.id.is_empty_blob() {
        Vec::new()
    } else {
        repo.find_object(entry.id)?.detach().data
    };
    Ok((convert_to_worktree(&repo, &file, data)?, entry.id))
}

/// Replaces the content of `file`'s git index entry, which must still have the blob `expected`,
/// with `content` (in its worktree form), and returns the new blob id. The index stays locked from reading it to writing
/// it back, like `git add`, and is refused for entries and indexes that cannot be updated without
/// git's own conversions or extensions.
pub fn update_index_entry(
    file: &Path,
    content: &[u8],
    expected: &str,
    trust_full: bool,
) -> Result<ObjectId> {
    use gix::index::entry::Stat;

    let file = gix::path::realpath(file).context("resolve symlinks")?;
    let repo = open_repo(get_repo_dir(&file)?, trust_full)
        .context("failed to open git repo")?
        .to_thread_local();
    let work_dir = repo.workdir().context("repo has no worktree")?;
    let rela = file.strip_prefix(work_dir)?.to_owned();
    let rela_path = gix::path::to_unix_separators_on_windows(gix::path::try_into_bstr(&rela)?);
    let expected = ObjectId::from_hex(expected.as_bytes())?;

    let index_path = repo.index_path();
    let lock = gix::lock::File::acquire_to_update_resource(
        &index_path,
        gix::lock::acquire::Fail::Immediately,
        None,
    )
    .map_err(|_| {
        anyhow::anyhow!(
            "the git index is locked ({}.lock exists): another git command is running",
            index_path.display()
        )
    })?;
    let mut index = repo.open_index()?;
    // gix writes neither a split index nor its shared part, which would lose entries.
    if index.link().is_some() || index.is_sparse() {
        bail!("split or sparse git indexes are not supported; stage with git");
    }
    let range = index
        .entry_range(rela_path.as_ref())
        .with_context(|| format!("{} is not in the git index", file.display()))?;
    if range.len() != 1 {
        bail!("{} has unmerged entries in the git index", file.display());
    }
    let entry = &index.entries()[range.start];
    if entry.stage() != Stage::Unconflicted {
        bail!("{} has unmerged entries in the git index", file.display());
    }
    if entry.id != expected {
        bail!(
            "the git index entry of {} changed since it was read; try again",
            file.display()
        );
    }
    if entry
        .flags
        .intersects(Flags::INTENT_TO_ADD | Flags::ASSUME_VALID | Flags::SKIP_WORKTREE)
    {
        bail!(
            "{} is intent-to-add, assume-unchanged or skip-worktree in the git index; stage it with git",
            file.display()
        );
    }
    match entry.mode.to_tree_entry_mode().map(|mode| mode.kind()) {
        Some(EntryKind::Blob | EntryKind::BlobExecutable) => (),
        kind => bail!("index entry at {} is not a file: {kind:?}", file.display()),
    }

    // The content is stored as-is, so files git would convert on the way in or out are
    // refused rather than staged differently from `git add`.
    let unconverted = |what: &str| {
        anyhow::anyhow!(
            "git {what} {} (filters or line-ending conversion); stage it with git",
            file.display()
        )
    };
    let (mut pipeline, _) = repo.filter_pipeline(None)?;
    let mut to_git = Vec::new();
    pipeline
        .convert_to_git(content, &rela, &index)?
        .read_to_end(&mut to_git)?;
    if to_git != content {
        return Err(unconverted("converts"));
    }
    let raw = if entry.id.is_empty_blob() {
        Vec::new()
    } else {
        repo.find_object(entry.id)?.detach().data
    };
    let mut to_worktree = Vec::new();
    pipeline
        .convert_to_worktree(&raw, rela_path.as_ref(), Delay::Forbid)?
        .read_to_end(&mut to_worktree)?;
    if to_worktree != raw {
        return Err(unconverted("converts the checked-out"));
    }
    let id = repo.write_blob(content)?.detach();
    // Matching stat data tells git the file equals the entry; otherwise it must compare.
    let stat = if std::fs::read(&file)? == content {
        Stat::from_fs(&gix::index::fs::Metadata::from_path_no_follow(&file)?)?
    } else {
        Stat::default()
    };

    let entry = &mut index.entries_mut()[range.start];
    entry.id = id;
    entry.stat = stat;
    // The tree cache would still describe the old entry; git rebuilds it when needed.
    index.remove_tree();
    let mut out = std::io::BufWriter::new(lock);
    index.write_to(&mut out, gix::index::write::Options::default())?;
    out.into_inner()
        .map_err(|err| err.into_error())?
        .commit()
        .map_err(|err| err.error)?;
    Ok(id)
}

/// Stages the worktree state of `paths` (files of the repository at `cwd`) like
/// `git add -A -- <paths>`: present files get their content (new ones a new entry), missing ones
/// lose their entry. Writes the index once and returns how many paths changed in it.
pub fn stage_paths(cwd: &Path, paths: &[std::path::PathBuf], trust_full: bool) -> Result<usize> {
    use gix::index::entry::{Mode, Stat};

    let repo = open_repo(cwd, trust_full)
        .context("failed to open git repo")?
        .to_thread_local();
    let work_dir = repo.workdir().context("repo has no worktree")?.to_owned();
    let index_path = repo.index_path();
    let lock = gix::lock::File::acquire_to_update_resource(
        &index_path,
        gix::lock::acquire::Fail::Immediately,
        None,
    )
    .map_err(|_| {
        anyhow::anyhow!(
            "the git index is locked ({}.lock exists): another git command is running",
            index_path.display()
        )
    })?;
    let mut index = repo.open_index()?;
    if index.link().is_some() || index.is_sparse() {
        bail!("split or sparse git indexes are not supported; stage with git");
    }
    let (mut pipeline, _) = repo.filter_pipeline(None)?;
    let mut changed = 0;
    let mut removed = Vec::new();
    for path in paths {
        let rela = path.strip_prefix(&work_dir)?.to_owned();
        let rela_path = gix::path::to_unix_separators_on_windows(gix::path::try_into_bstr(&rela)?);
        let range = index.entry_range(rela_path.as_ref());
        if let Some(range) = &range {
            let entries = &index.entries()[range.clone()];
            if entries.len() != 1 || entries[0].stage() != Stage::Unconflicted {
                bail!("{} has unmerged entries in the git index", path.display());
            }
        }
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            if range.is_some() {
                removed.push(rela_path.into_owned());
                changed += 1;
            }
            continue;
        };
        if !meta.is_file() {
            bail!(
                "{} is not a regular file; stage it with git",
                path.display()
            );
        }
        let content = std::fs::read(path)?;
        let mut to_git = Vec::new();
        pipeline
            .convert_to_git(&content[..], &rela, &index)?
            .read_to_end(&mut to_git)?;
        if to_git != content {
            bail!(
                "git converts {} (filters or line-ending conversion); stage it with git",
                path.display()
            );
        }
        let id = repo.write_blob(&content)?.detach();
        let stat = Stat::from_fs(&gix::index::fs::Metadata::from_path_no_follow(path)?)?;
        #[cfg(unix)]
        let executable = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
        #[cfg(not(unix))]
        let executable = false;
        let mode = if executable {
            Mode::FILE_EXECUTABLE
        } else {
            Mode::FILE
        };
        match range {
            Some(range) => {
                let entry = &mut index.entries_mut()[range.start];
                if entry.id != id || entry.mode != mode {
                    changed += 1;
                }
                entry.id = id;
                entry.stat = stat;
                entry.mode = mode;
                entry.flags.remove(Flags::INTENT_TO_ADD);
            }
            None => {
                index.dangerously_push_entry(stat, id, Flags::empty(), mode, rela_path.as_ref());
                changed += 1;
            }
        }
    }
    index.remove_entries(|_, path, _| removed.iter().any(|removed| removed == path));
    index.sort_entries();
    index.remove_tree();
    let mut out = std::io::BufWriter::new(lock);
    index.write_to(&mut out, gix::index::write::Options::default())?;
    out.into_inner()
        .map_err(|err| err.into_error())?
        .commit()
        .map_err(|err| err.error)?;
    Ok(changed)
}

pub fn get_commit_base(file: &Path, revision: &str, trust_full: bool) -> Result<CommitBase> {
    let file = gix::path::realpath(file).context("resolve symlinks")?;
    let repo = open_repo(get_repo_dir(&file)?, trust_full)?.to_thread_local();
    let commit = repo
        .rev_parse_single(revision)?
        .object()?
        .peel_to_commit()?;
    let blob_id = find_file_in_commit(&repo, &commit, &file)?;
    let data = match blob_id {
        Some(id) => convert_to_worktree(&repo, &file, repo.find_object(id)?.detach().data)?,
        None => Vec::new(),
    };
    Ok(CommitBase {
        data,
        commit_id: commit.id.to_hex().to_string(),
        blob_id: blob_id.map(|id| id.to_hex().to_string()),
    })
}

pub fn get_commit_history(file: &Path, trust_full: bool) -> Result<Vec<CommitInfo>> {
    let file = gix::path::realpath(file).context("resolve symlinks")?;
    let repo = open_repo(get_repo_dir(&file)?, trust_full)?.to_thread_local();
    let walk = repo
        .head_id()?
        .ancestors()
        .sorting(gix::revision::walk::Sorting::ByCommitTime(
            gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
        ))
        .all()?;
    walk.take(200)
        .map(|entry| {
            let commit = entry?.object()?;
            Ok(CommitInfo {
                id: commit.id.to_hex().to_string(),
                summary: commit.message()?.summary().to_string(),
            })
        })
        .collect()
}

fn convert_to_worktree(repo: &Repository, file: &Path, data: Vec<u8>) -> Result<Vec<u8>> {
    // Get the actual data that git would make out of the git object.
    // This will apply the user's git config or attributes like crlf conversions.
    //
    // The whole filter pipeline still runs in untrusted (`Trust::Reduced`) mode so built-in
    // conversions like autocrlf keep working, but gix drops `filter.*.clean` / `filter.*.smudge`
    // drivers defined in untrusted (repository-local) config, so those external programs are not
    // executed unless the workspace was explicitly trusted. This relies on `open_repo` forcing the
    // trust level instead of letting gix re-derive it from `.git` ownership; see the note there.
    if let Some(work_dir) = repo.workdir() {
        let rela_path = file.strip_prefix(work_dir)?;
        let rela_path = gix::path::try_into_bstr(rela_path)?;
        let (mut pipeline, _) = repo.filter_pipeline(None)?;
        let mut worktree_outcome =
            pipeline.convert_to_worktree(&data, rela_path.as_ref(), Delay::Forbid)?;
        let mut buf = Vec::with_capacity(data.len());
        worktree_outcome.read_to_end(&mut buf)?;
        Ok(buf)
    } else {
        Ok(data)
    }
}

pub fn get_current_head_name(file: &Path, trust_full: bool) -> Result<Arc<ArcSwap<Box<str>>>> {
    debug_assert!(!file.exists() || file.is_file());
    debug_assert!(file.is_absolute());
    let file = gix::path::realpath(file).context("resolve symlinks")?;

    let repo_dir = get_repo_dir(&file)?;
    let repo = open_repo(repo_dir, trust_full)
        .context("failed to open git repo")?
        .to_thread_local();
    let head_ref = repo.head_ref()?;
    let head_commit = repo.head_commit()?;

    let name = match head_ref {
        Some(reference) => reference.name().shorten().to_string(),
        None => head_commit.id.to_hex_with_len(8).to_string(),
    };

    Ok(Arc::new(ArcSwap::from_pointee(name.into_boxed_str())))
}

pub fn for_each_changed_file(
    cwd: &Path,
    trust_full: bool,
    f: impl Fn(Result<FileChange>) -> bool,
) -> Result<()> {
    status(&open_repo(cwd, trust_full)?.to_thread_local(), f)
}

/// The `HEAD` commit and the git index file's size and modification time of the repository
/// containing `cwd`: they change with every commit, checkout and staging.
pub fn repo_state(cwd: &Path, trust_full: bool) -> Result<String> {
    let repo = open_repo(cwd, trust_full)?.to_thread_local();
    let head = repo.head_id().map(|id| id.to_hex().to_string());
    let index = std::fs::metadata(repo.index_path())
        .ok()
        .map(|meta| (meta.len(), meta.modified().ok()));
    Ok(format!("{head:?} {index:?}"))
}

pub fn resolve_commit(cwd: &Path, revision: &str, trust_full: bool) -> Result<String> {
    let repo = open_repo(cwd, trust_full)?.to_thread_local();
    let commit = repo
        .rev_parse_single(revision)?
        .object()?
        .peel_to_commit()?;
    Ok(commit.id.to_hex().to_string())
}

/// The commit one step older (the first parent) or newer (the next commit on the ancestry path
/// to `HEAD`) than `commit`, and how many commits it is behind `HEAD`.
pub fn step_commit(
    cwd: &Path,
    commit: &str,
    older: bool,
    trust_full: bool,
) -> Result<(CommitInfo, usize)> {
    let repo = open_repo(cwd, trust_full)?.to_thread_local();
    let base = ObjectId::from_hex(commit.as_bytes())?;
    let head = repo.head_id()?.detach();
    let target = if older {
        repo.find_commit(base)?
            .parent_ids()
            .next()
            .with_context(|| format!("{} has no parent", &commit[..8]))?
            .detach()
    } else {
        if base == head {
            bail!("already at HEAD");
        }
        // The commits after `base` up to HEAD, newest first; the last child of `base` is next.
        let mut next = None;
        for info in repo.rev_walk([head]).with_hidden([base]).all()? {
            let info = info?;
            if info.parent_ids().any(|parent| parent == base) {
                next = Some(info.id);
            }
        }
        next.with_context(|| format!("{} is not an ancestor of HEAD", &commit[..8]))?
    };
    let behind = repo.rev_walk([head]).with_hidden([target]).all()?.count();
    let summary = repo.find_commit(target)?.message()?.summary().to_string();
    Ok((
        CommitInfo {
            id: target.to_hex().to_string(),
            summary,
        },
        behind,
    ))
}

/// Up to `limit` commits of the first-parent history of `HEAD`, newest first.
pub fn first_parent_log(cwd: &Path, limit: usize, trust_full: bool) -> Result<Vec<CommitEntry>> {
    let repo = open_repo(cwd, trust_full)?.to_thread_local();
    let walk = repo
        .rev_walk([repo.head_id()?.detach()])
        .first_parent_only()
        .all()?;
    walk.take(limit)
        .map(|info| {
            let commit = info?.object()?;
            Ok(CommitEntry {
                id: commit.id.to_hex().to_string(),
                summary: commit.message()?.summary().to_string(),
                author: commit.author()?.name.to_string(),
                time: commit.time()?.seconds,
            })
        })
        .collect()
}

/// Whether `commit` is `HEAD` or one of its ancestors in the repository at `cwd`.
pub fn is_in_head_history(cwd: &Path, commit: &str, trust_full: bool) -> Result<bool> {
    let repo = open_repo(cwd, trust_full)?.to_thread_local();
    let commit = ObjectId::from_hex(commit.as_bytes())?;
    let head = repo.head_id()?.detach();
    Ok(commit == head
        || repo
            .merge_base(commit, head)
            .is_ok_and(|base| base.detach() == commit))
}

/// The merge base of `revision` and `HEAD` in the repository at `cwd`.
pub fn merge_base_with_head(cwd: &Path, revision: &str, trust_full: bool) -> Result<String> {
    let repo = open_repo(cwd, trust_full)?.to_thread_local();
    let other = repo.rev_parse_single(revision)?.detach();
    let head = repo.head_id()?.detach();
    Ok(repo.merge_base(other, head)?.to_hex().to_string())
}

/// Whether the git index of the repository at `cwd` differs from `HEAD` for any path.
pub fn has_staged_changes(cwd: &Path, trust_full: bool) -> Result<bool> {
    let repo = open_repo(cwd, trust_full)?.to_thread_local();
    let tree = repo.head_commit()?.tree_id()?;
    let status = repo
        .status(gix::progress::Discard)?
        .untracked_files(UntrackedFiles::None)
        .head_tree(tree)
        .tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled)
        .into_iter(Vec::new())?;
    for item in status {
        if let gix::status::Item::TreeIndex(_) = item? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn get_changed_files_since(
    cwd: &Path,
    commit: &str,
    trust_full: bool,
) -> Result<Vec<FileChange>> {
    let repo = open_repo(cwd, trust_full)?.to_thread_local();
    let work_dir = repo
        .workdir()
        .context("repo has no worktree")?
        .to_path_buf();
    let tree = repo
        .rev_parse_single(commit)?
        .object()?
        .peel_to_commit()?
        .tree_id()?;
    let status = repo
        .status(gix::progress::Discard)?
        .untracked_files(UntrackedFiles::Files)
        .head_tree(tree)
        .tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled)
        .into_iter(Vec::new())?;

    // Per path, the change from the commit to the index and from the index to the worktree.
    let mut changes: BTreeMap<BString, (Option<char>, Option<char>)> = BTreeMap::new();
    for item in status {
        match item? {
            gix::status::Item::TreeIndex(change) => {
                let kind = match change {
                    gix::diff::index::Change::Addition { .. } => 'A',
                    gix::diff::index::Change::Deletion { .. } => 'D',
                    _ => 'M',
                };
                changes.entry(change.location().to_owned()).or_default().0 = Some(kind);
            }
            gix::status::Item::IndexWorktree(Item::Modification {
                rela_path, status, ..
            }) => {
                let kind = match status {
                    EntryStatus::Change(Change::Removed) => 'D',
                    EntryStatus::IntentToAdd => '?',
                    _ => 'M',
                };
                changes.entry(rela_path).or_default().1 = Some(kind);
            }
            gix::status::Item::IndexWorktree(Item::DirectoryContents { entry, .. })
                if entry.status == Status::Untracked =>
            {
                changes.entry(entry.rela_path).or_default().1 = Some('?');
            }
            _ => (),
        }
    }

    let mut files = Vec::new();
    for (rela_path, (index, worktree)) in changes {
        let in_commit = index.map_or(worktree != Some('?'), |kind| kind != 'A');
        let in_worktree = worktree.map_or(index != Some('D'), |kind| kind != 'D');
        let path = work_dir.join(rela_path.to_path()?);
        files.push(match (in_commit, in_worktree) {
            (true, true) => FileChange::Modified { path },
            (true, false) => FileChange::Deleted { path },
            (false, true) => FileChange::Untracked { path },
            (false, false) => continue,
        });
    }
    Ok(files)
}

fn open_repo(path: &Path, trust_full: bool) -> Result<ThreadSafeRepository> {
    // `trust_full` is the workspace-trust decision made by the caller, and it must be the
    // authority on the gix trust level. gix's own discovery (`discover_*`) ignores a
    // caller-supplied trust level: it always re-derives trust from `.git` ownership, so a malicious
    // `.git/config` in a user-owned directory would be opened as `Trust::Full` regardless of our
    // gate. Worse, the GIT_DIR-environment branch of that discovery panics because it never sets a
    // trust level at all. So we split discovery from opening: find the repository path ourselves,
    // then `open_opts(..).with(trust)`, which forces the trust level and skips gix's ownership
    // check. Under `Trust::Reduced`, gix then refuses to honor untrusted repository-local config
    // such as `filter.*` smudge/clean drivers.

    let trust = if trust_full {
        gix::sec::Trust::Full
    } else {
        gix::sec::Trust::Reduced
    };

    // On Windows various configuration options are bundled as part of the git installation. The
    // lookup is expensive; only do it there.
    let config = gix::open::permissions::Config {
        system: true,
        git: true,
        user: true,
        env: true,
        includes: true,
        git_binary: cfg!(windows),
    };

    let permissions = gix::open::Permissions {
        config,
        ..gix::open::Permissions::default_for_level(trust)
    };

    let discover_options = gix::discover::upwards::Options {
        dot_git_only: true,
        ..Default::default()
    };
    let (repo_path, _trust_from_ownership) = gix::discover::upwards_opts(path, discover_options)
        .context("failed to discover git repo")?;
    let (git_dir, _work_dir) = repo_path.into_repository_and_work_tree_directories();

    // Opening parses every config file, which review navigation would otherwise do dozens of
    // times per key press. Refs, objects and the index are read from disk (or checked for
    // staleness) on use, so a recently opened repository stays valid while its config does.
    let config_time = |repo: &ThreadSafeRepository| {
        // The objects directory is in the common directory, which linked worktrees share.
        let common = repo.objects_dir().parent().unwrap_or(repo.git_dir());
        std::fs::metadata(common.join("config"))
            .and_then(|meta| meta.modified())
            .ok()
    };
    let cached = OPENED.with_borrow_mut(|opened| {
        let idx = opened.iter().position(|(dir, trust_full, _, _)| {
            *dir == git_dir && *trust_full == (trust == gix::sec::Trust::Full)
        })?;
        let entry = opened.remove(idx);
        if entry.2 != config_time(&entry.3) {
            return None;
        }
        let repo = entry.3.clone();
        opened.insert(0, entry);
        Some(repo)
    });
    if let Some(repo) = cached {
        return Ok(repo);
    }

    let options = gix::open::Options::default()
        .permissions(permissions)
        // `git_dir` is the discovered `.git` directory (or a linked-worktree git dir), so open it
        // as-is rather than letting gix append `.git` again.
        .open_path_as_is(true)
        .with(trust);

    let repo = ThreadSafeRepository::open_opts(git_dir.clone(), options)?;
    OPENED.with_borrow_mut(|opened| {
        opened.insert(
            0,
            (
                git_dir,
                trust == gix::sec::Trust::Full,
                config_time(&repo),
                repo.clone(),
            ),
        );
        opened.truncate(4);
    });
    Ok(repo)
}

type OpenedRepo = (
    std::path::PathBuf,
    bool,
    Option<std::time::SystemTime>,
    ThreadSafeRepository,
);

thread_local! {
    /// The repositories this thread opened last, most recent first: by git dir and full trust,
    /// with their config's modification time when opened.
    static OPENED: std::cell::RefCell<Vec<OpenedRepo>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Emulates the result of running `git status` from the command line.
fn status(repo: &Repository, f: impl Fn(Result<FileChange>) -> bool) -> Result<()> {
    let work_dir = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("working tree not found"))?
        .to_path_buf();

    let status_platform = repo
        .status(gix::progress::Discard)?
        // Here we discard the `status.showUntrackedFiles` config, as it makes little sense in
        // our case to not list new (untracked) files. We could have respected this config
        // if the default value weren't `Collapsed` though, as this default value would render
        // the feature unusable to many.
        .untracked_files(UntrackedFiles::Files)
        // Turn on file rename detection, which is off by default.
        .index_worktree_rewrites(Some(Rewrites {
            copies: None,
            percentage: Some(0.5),
            limit: 1000,
            ..Default::default()
        }));

    // No filtering based on path
    let empty_patterns = vec![];

    let status_iter = status_platform.into_index_worktree_iter(empty_patterns)?;

    for item in status_iter {
        let Ok(item) = item.map_err(|err| f(Err(err.into()))) else {
            continue;
        };
        let change = match item {
            Item::Modification {
                rela_path, status, ..
            } => {
                let path = work_dir.join(rela_path.to_path()?);
                match status {
                    EntryStatus::Conflict { .. } => FileChange::Conflict { path },
                    EntryStatus::Change(Change::Removed) => FileChange::Deleted { path },
                    EntryStatus::Change(Change::Modification { .. }) => {
                        FileChange::Modified { path }
                    }
                    // Files marked with `git add --intent-to-add`. Such files
                    // still show up as new in `git status`, so it's appropriate
                    // to show them the same way as untracked files in the
                    // "changed file" picker. One example of this being used
                    // is Jujutsu, a Git-compatible VCS. It marks all new files
                    // with `--intent-to-add` automatically.
                    EntryStatus::IntentToAdd => FileChange::Untracked { path },
                    _ => continue,
                }
            }
            Item::DirectoryContents { entry, .. } if entry.status == Status::Untracked => {
                FileChange::Untracked {
                    path: work_dir.join(entry.rela_path.to_path()?),
                }
            }
            Item::Rewrite {
                source,
                dirwalk_entry,
                ..
            } => FileChange::Renamed {
                from_path: work_dir.join(source.rela_path().to_path()?),
                to_path: work_dir.join(dirwalk_entry.rela_path.to_path()?),
            },
            _ => continue,
        };
        if !f(Ok(change)) {
            break;
        }
    }

    Ok(())
}

/// Finds the object that contains the contents of a file at a specific commit.
fn find_file_in_commit(
    repo: &Repository,
    commit: &Commit,
    file: &Path,
) -> Result<Option<ObjectId>> {
    let repo_dir = repo.workdir().context("repo has no worktree")?;
    let rel_path = file.strip_prefix(repo_dir)?;
    let tree = commit.tree()?;
    let Some(tree_entry) = tree.lookup_entry_by_path(rel_path)? else {
        return Ok(None);
    };
    match tree_entry.mode().kind() {
        // not a file, everything is new, do not show diff
        mode @ (EntryKind::Tree | EntryKind::Commit | EntryKind::Link) => {
            bail!("entry at {} is not a file but a {mode:?}", file.display())
        }
        // found a file
        EntryKind::Blob | EntryKind::BlobExecutable => Ok(Some(tree_entry.object_id())),
    }
}
