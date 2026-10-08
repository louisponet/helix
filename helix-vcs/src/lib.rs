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

pub use diff::{apply_hunk, compute_hunks, line_hunks, unified_diff, DiffHandle, Hunk};

mod status;

pub use status::FileChange;

pub struct CommitBase {
    pub data: Vec<u8>,
    pub commit_id: String,
    pub blob_id: Option<String>,
}

/// The new content of a git index entry, see [`DiffProviderRegistry::update_index_entry`].
#[derive(Clone, Copy, Debug)]
pub enum IndexUpdate<'a> {
    /// Stage these bytes, which must be what the file holds in the worktree.
    Content(&'a [u8]),
    /// Restore an existing blob, given by its hex id.
    Blob(&'a str),
}

pub struct CommitInfo {
    pub id: String,
    pub summary: String,
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

    /// Replaces the git index entry of `file`, which must still have the blob `expected`, with
    /// `update`. Returns the new blob id.
    pub fn update_index_entry(
        &self,
        file: &Path,
        update: IndexUpdate,
        expected: &str,
        trust_full: bool,
    ) -> Result<String> {
        let mut first_err = None;
        for provider in &self.providers {
            match provider.update_index_entry(file, update, expected, trust_full) {
                Ok(id) => return Ok(id),
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
        update: IndexUpdate,
        expected: &str,
        trust_full: bool,
    ) -> Result<String> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::update_index_entry(file, update, expected, trust_full)
                .map(|id| id.to_hex().to_string()),
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
