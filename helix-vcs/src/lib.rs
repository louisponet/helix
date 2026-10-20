//! `helix_vcs` provides types for working with diffs from a Version Control System (VCS).
//! Currently `git` is the only supported provider for diffs, but this architecture allows
//! for other providers to be added in the future.

use anyhow::{anyhow, bail, Result};
use arc_swap::ArcSwap;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(feature = "git")]
mod git;

mod diff;

pub use diff::{
    apply_hunk, compute_hunks, find_hunk, line_hunks, staged_hunk_count, unified_diff, DiffHandle,
    Hunk,
};

mod status;

pub use status::FileChange;

/// The git blob id of `data`, `None` without git support.
pub fn blob_id(data: &[u8]) -> Option<String> {
    #[cfg(feature = "git")]
    return gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, data)
        .ok()
        .map(|id| id.to_hex().to_string());
    #[cfg(not(feature = "git"))]
    {
        let _ = data;
        None
    }
}

pub struct CommitBase {
    pub data: Vec<u8>,
    pub commit_id: String,
    pub blob_id: Option<String>,
}

pub struct CommitInfo {
    pub id: String,
    pub summary: String,
}

/// A commit of the first-parent history of `HEAD`, for the baseline picker.
pub struct CommitEntry {
    pub id: String,
    pub summary: String,
    pub author: String,
    /// The commit time in seconds since the Unix epoch.
    pub time: i64,
}

/// Contains all active diff providers. Diff providers are compiled in via features. Currently
/// only `git` is supported.
#[derive(Clone)]
pub struct DiffProviderRegistry {
    providers: Vec<DiffProvider>,
}

impl DiffProviderRegistry {
    /// Get the given file from the VCS. This provides the unedited document as a "base"
    /// for a diff to be created.
    pub fn get_diff_base(&self, file: &Path, trust_full: bool) -> Option<Vec<u8>> {
        self.providers
            .iter()
            .find_map(|provider| match provider.get_diff_base(file, trust_full) {
                Ok(res) => Some(res),
                Err(err) => {
                    log::debug!("{err:#?}");
                    log::debug!("failed to open diff base for {}", file.display());
                    None
                }
            })
    }

    /// Get the given file's content staged in the VCS index together with a hex identifier of
    /// that content. Unlike [`Self::get_diff_base`] the reason for a failure is returned.
    pub fn get_index_base(&self, file: &Path, trust_full: bool) -> Result<(Vec<u8>, String)> {
        let mut first_err = None;
        for provider in &self.providers {
            match provider.get_index_base(file, trust_full) {
                Ok(res) => return Ok(res),
                Err(err) => {
                    first_err.get_or_insert(err);
                }
            }
        }
        Err(first_err.unwrap_or_else(|| anyhow!("no diff provider available")))
    }

    /// Replaces the content of the git index entry of `file`, which must still have the blob
    /// `expected`, with `content` (in its worktree form). Returns the new blob id.
    pub fn update_index_entry(
        &self,
        file: &Path,
        content: &[u8],
        expected: &str,
        trust_full: bool,
    ) -> Result<String> {
        let mut first_err = None;
        for provider in &self.providers {
            match provider.update_index_entry(file, content, expected, trust_full) {
                Ok(id) => return Ok(id),
                Err(err) => {
                    first_err.get_or_insert(err);
                }
            }
        }
        Err(first_err.unwrap_or_else(|| anyhow!("no diff provider available")))
    }

    /// Whether the git index of the repository at `cwd` differs from `HEAD` for any path.
    pub fn has_staged_changes(&self, cwd: &Path, trust_full: bool) -> Result<bool> {
        let mut first_err = None;
        for provider in &self.providers {
            match provider.has_staged_changes(cwd, trust_full) {
                Ok(staged) => return Ok(staged),
                Err(err) => {
                    first_err.get_or_insert(err);
                }
            }
        }
        Err(first_err.unwrap_or_else(|| anyhow!("no diff provider available")))
    }

    pub fn get_commit_base(
        &self,
        file: &Path,
        revision: &str,
        trust_full: bool,
    ) -> Result<CommitBase> {
        let mut first_err = None;
        for provider in &self.providers {
            match provider.get_commit_base(file, revision, trust_full) {
                Ok(base) => return Ok(base),
                Err(err) => {
                    first_err.get_or_insert(err);
                }
            }
        }
        Err(first_err.unwrap_or_else(|| anyhow!("no diff provider available")))
    }

    pub fn get_commit_history(&self, file: &Path, trust_full: bool) -> Result<Vec<CommitInfo>> {
        let mut first_err = None;
        for provider in &self.providers {
            match provider.get_commit_history(file, trust_full) {
                Ok(commits) => return Ok(commits),
                Err(err) => {
                    first_err.get_or_insert(err);
                }
            }
        }
        Err(first_err.unwrap_or_else(|| anyhow!("no diff provider available")))
    }

    /// Up to `limit` commits of the first-parent history of `HEAD`, newest first.
    pub fn first_parent_log(
        &self,
        cwd: &Path,
        limit: usize,
        trust_full: bool,
    ) -> Result<Vec<CommitEntry>> {
        let mut first_err = None;
        for provider in &self.providers {
            match provider.first_parent_log(cwd, limit, trust_full) {
                Ok(log) => return Ok(log),
                Err(err) => {
                    first_err.get_or_insert(err);
                }
            }
        }
        Err(first_err.unwrap_or_else(|| anyhow!("no diff provider available")))
    }

    /// Whether `commit` is `HEAD` or one of its ancestors in the repository at `cwd`.
    pub fn is_in_head_history(&self, cwd: &Path, commit: &str, trust_full: bool) -> Result<bool> {
        let mut first_err = None;
        for provider in &self.providers {
            match provider.is_in_head_history(cwd, commit, trust_full) {
                Ok(found) => return Ok(found),
                Err(err) => {
                    first_err.get_or_insert(err);
                }
            }
        }
        Err(first_err.unwrap_or_else(|| anyhow!("no diff provider available")))
    }

    /// The merge base of `revision` and `HEAD` in the repository at `cwd`.
    pub fn merge_base_with_head(
        &self,
        cwd: &Path,
        revision: &str,
        trust_full: bool,
    ) -> Result<String> {
        let mut first_err = None;
        for provider in &self.providers {
            match provider.merge_base_with_head(cwd, revision, trust_full) {
                Ok(base) => return Ok(base),
                Err(err) => {
                    first_err.get_or_insert(err);
                }
            }
        }
        Err(first_err.unwrap_or_else(|| anyhow!("no diff provider available")))
    }

    /// Stages the worktree state of `paths` in the repository at `cwd` like `git add -A` limited
    /// to them, returning how many paths changed in the index.
    pub fn stage_paths(&self, cwd: &Path, paths: &[PathBuf], trust_full: bool) -> Result<usize> {
        let mut first_err = None;
        for provider in &self.providers {
            match provider.stage_paths(cwd, paths, trust_full) {
                Ok(count) => return Ok(count),
                Err(err) => {
                    first_err.get_or_insert(err);
                }
            }
        }
        Err(first_err.unwrap_or_else(|| anyhow!("no diff provider available")))
    }

    /// The commit one step older or newer than `commit` towards `HEAD`, with how many commits it is
    /// behind `HEAD`.
    pub fn step_commit(
        &self,
        cwd: &Path,
        commit: &str,
        older: bool,
        trust_full: bool,
    ) -> Result<(CommitInfo, usize)> {
        let mut first_err = None;
        for provider in &self.providers {
            match provider.step_commit(cwd, commit, older, trust_full) {
                Ok(step) => return Ok(step),
                Err(err) => {
                    first_err.get_or_insert(err);
                }
            }
        }
        Err(first_err.unwrap_or_else(|| anyhow!("no diff provider available")))
    }

    /// A summary of the repository containing `cwd` (its `HEAD` and git index file) that
    /// changes with every commit, checkout and staging.
    pub fn repo_state(&self, cwd: &Path, trust_full: bool) -> Result<String> {
        let mut first_err = None;
        for provider in &self.providers {
            match provider.repo_state(cwd, trust_full) {
                Ok(state) => return Ok(state),
                Err(err) => {
                    first_err.get_or_insert(err);
                }
            }
        }
        Err(first_err.unwrap_or_else(|| anyhow!("no diff provider available")))
    }

    /// Resolves `revision` in the repository containing `cwd` to the hex id of a commit.
    pub fn resolve_commit(&self, cwd: &Path, revision: &str, trust_full: bool) -> Result<String> {
        let mut first_err = None;
        for provider in &self.providers {
            match provider.resolve_commit(cwd, revision, trust_full) {
                Ok(id) => return Ok(id),
                Err(err) => {
                    first_err.get_or_insert(err);
                }
            }
        }
        Err(first_err.unwrap_or_else(|| anyhow!("no diff provider available")))
    }

    /// Lists the files of the worktree containing `cwd` that differ from `commit`, staged or
    /// not. Files added since `commit` are reported as [`FileChange::Untracked`].
    pub fn get_changed_files_since(
        &self,
        cwd: &Path,
        commit: &str,
        trust_full: bool,
    ) -> Result<Vec<FileChange>> {
        let mut first_err = None;
        for provider in &self.providers {
            match provider.get_changed_files_since(cwd, commit, trust_full) {
                Ok(changes) => return Ok(changes),
                Err(err) => {
                    first_err.get_or_insert(err);
                }
            }
        }
        Err(first_err.unwrap_or_else(|| anyhow!("no diff provider available")))
    }

    /// Get the current name of the current [HEAD](https://stackoverflow.com/questions/2304087/what-is-head-in-git).
    pub fn get_current_head_name(
        &self,
        file: &Path,
        trust_full: bool,
    ) -> Option<Arc<ArcSwap<Box<str>>>> {
        self.providers.iter().find_map(|provider| {
            match provider.get_current_head_name(file, trust_full) {
                Ok(res) => Some(res),
                Err(err) => {
                    log::debug!("{err:#?}");
                    log::debug!("failed to obtain current head name for {}", file.display());
                    None
                }
            }
        })
    }

    /// Fire-and-forget changed file iteration. Runs everything in a background task. Keeps
    /// iteration until `on_change` returns `false`.
    pub fn for_each_changed_file(
        self,
        cwd: PathBuf,
        trust_full: bool,
        f: impl Fn(Result<FileChange>) -> bool + Send + 'static,
    ) {
        tokio::task::spawn_blocking(move || {
            if self
                .providers
                .iter()
                .find_map(|provider| provider.for_each_changed_file(&cwd, trust_full, &f).ok())
                .is_none()
            {
                f(Err(anyhow!("no diff provider returns success")));
            }
        });
    }
}

impl Default for DiffProviderRegistry {
    fn default() -> Self {
        // currently only git is supported
        // TODO make this configurable when more providers are added
        let providers = vec![
            #[cfg(feature = "git")]
            DiffProvider::Git,
            DiffProvider::None,
        ];
        DiffProviderRegistry { providers }
    }
}

/// A union type that includes all types that implement [DiffProvider]. We need this type to allow
/// cloning [DiffProviderRegistry] as `Clone` cannot be used in trait objects.
///
/// `Copy` is simply to ensure the `clone()` call is the simplest it can be.
#[derive(Copy, Clone)]
enum DiffProvider {
    #[cfg(feature = "git")]
    Git,
    None,
}

impl DiffProvider {
    fn get_diff_base(&self, file: &Path, trust_full: bool) -> Result<Vec<u8>> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::get_diff_base(file, trust_full),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn get_index_base(&self, file: &Path, trust_full: bool) -> Result<(Vec<u8>, String)> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::get_index_base(file, trust_full)
                .map(|(data, oid)| (data, oid.to_hex().to_string())),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn update_index_entry(
        &self,
        file: &Path,
        content: &[u8],
        expected: &str,
        trust_full: bool,
    ) -> Result<String> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::update_index_entry(file, content, expected, trust_full)
                .map(|id| id.to_hex().to_string()),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn has_staged_changes(&self, cwd: &Path, trust_full: bool) -> Result<bool> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::has_staged_changes(cwd, trust_full),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn get_commit_base(&self, file: &Path, revision: &str, trust_full: bool) -> Result<CommitBase> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::get_commit_base(file, revision, trust_full),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn get_commit_history(&self, file: &Path, trust_full: bool) -> Result<Vec<CommitInfo>> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::get_commit_history(file, trust_full),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn first_parent_log(
        &self,
        cwd: &Path,
        limit: usize,
        trust_full: bool,
    ) -> Result<Vec<CommitEntry>> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::first_parent_log(cwd, limit, trust_full),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn is_in_head_history(&self, cwd: &Path, commit: &str, trust_full: bool) -> Result<bool> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::is_in_head_history(cwd, commit, trust_full),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn merge_base_with_head(&self, cwd: &Path, revision: &str, trust_full: bool) -> Result<String> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::merge_base_with_head(cwd, revision, trust_full),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn stage_paths(&self, cwd: &Path, paths: &[PathBuf], trust_full: bool) -> Result<usize> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::stage_paths(cwd, paths, trust_full),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn step_commit(
        &self,
        cwd: &Path,
        commit: &str,
        older: bool,
        trust_full: bool,
    ) -> Result<(CommitInfo, usize)> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::step_commit(cwd, commit, older, trust_full),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn repo_state(&self, cwd: &Path, trust_full: bool) -> Result<String> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::repo_state(cwd, trust_full),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn resolve_commit(&self, cwd: &Path, revision: &str, trust_full: bool) -> Result<String> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::resolve_commit(cwd, revision, trust_full),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn get_changed_files_since(
        &self,
        cwd: &Path,
        commit: &str,
        trust_full: bool,
    ) -> Result<Vec<FileChange>> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::get_changed_files_since(cwd, commit, trust_full),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn get_current_head_name(
        &self,
        file: &Path,
        trust_full: bool,
    ) -> Result<Arc<ArcSwap<Box<str>>>> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::get_current_head_name(file, trust_full),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn for_each_changed_file(
        &self,
        cwd: &Path,
        trust_full: bool,
        f: impl Fn(Result<FileChange>) -> bool,
    ) -> Result<()> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::for_each_changed_file(cwd, trust_full, f),
            Self::None => bail!("No diff support compiled in"),
        }
    }
}
