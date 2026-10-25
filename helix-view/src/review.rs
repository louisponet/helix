//! Inline review of a document against an index or commit snapshot.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::ops;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use helix_core::doc_formatter::TextFormat;
use helix_core::graphemes::prev_grapheme_boundary;
use helix_core::line_ending::line_end_char_index;
use helix_core::text_annotations::{LineAnnotation, TextAnnotations};
use helix_core::{
    softwrapped_dimensions, visual_offset_from_block, Position, Range, Rope, RopeSlice,
};
use helix_vcs::{compute_hunks, Hunk};
use serde::{Deserialize, Serialize};

use crate::ViewId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ReviewBaseline {
    GitIndex {
        blob_oid: String,
    },
    GitCommit {
        commit_oid: String,
        blob_oid: Option<String>,
    },
}

impl ReviewBaseline {
    pub fn revision(&self) -> Option<&str> {
        match self {
            Self::GitIndex { .. } => None,
            Self::GitCommit { commit_oid, .. } => Some(commit_oid),
        }
    }
}

impl std::fmt::Display for ReviewBaseline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GitIndex { .. } => f.write_str("git index"),
            Self::GitCommit { commit_oid, .. } => write!(f, "git commit {}", &commit_oid[..8]),
        }
    }
}

/// Lines of the baseline snapshot that were removed or replaced in the document. They are shown
/// as virtual lines below `anchor_line` of the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OldBlock {
    /// Line range in the baseline snapshot.
    pub old: ops::Range<usize>,
    /// Line range in the document that replaced the old lines.
    pub current: ops::Range<usize>,
    /// Document line the old lines are shown below.
    pub anchor_line: usize,
}

impl OldBlock {
    fn new(hunk: &Hunk) -> Self {
        let current = hunk.after.start as usize..hunk.after.end as usize;
        // Virtual lines can only follow a document line, so old lines from the start of the
        // file are shown (labelled) below the lines that replaced them, or below the first line.
        let anchor_line = match current.start.checked_sub(1) {
            Some(line) => line,
            None => current.end.saturating_sub(1),
        };
        OldBlock {
            old: hunk.before.start as usize..hunk.before.end as usize,
            current,
            anchor_line,
        }
    }

    /// The document line next to the old lines: the first replacing line, or the line the old
    /// lines are shown below if they were removed without replacement.
    pub fn adjacent_line(&self) -> usize {
        if self.current.is_empty() {
            self.anchor_line
        } else {
            self.current.start
        }
    }

    /// Whether the old lines are shown below the place they were removed from.
    pub fn is_displaced(&self) -> bool {
        self.current.start == 0
    }

    /// The label shown above displaced old lines.
    pub fn label(&self) -> Option<String> {
        if !self.is_displaced() {
            return None;
        }
        let span = |lines: &ops::Range<usize>| match lines.len() {
            1 => format!("line {}", lines.start + 1),
            _ => format!("lines {}-{}", lines.start + 1, lines.end),
        };
        Some(if self.current.is_empty() {
            format!("old {} removed before line 1:", span(&self.old))
        } else {
            format!(
                "old {} replaced by {} above:",
                span(&self.old),
                span(&self.current)
            )
        })
    }
}

/// The read-only selection of a view in the old text of a review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OldFocus {
    /// Selection in the baseline snapshot.
    pub range: Range,
    /// Whether movements extend the selection.
    pub extend: bool,
}

/// The files changed since the review baseline as review navigation sees them, kept in
/// `Editor::review_nav` while the repository and those files are unchanged on disk.
#[derive(Debug, Clone)]
pub struct ReviewNav {
    /// The baseline these files changed since.
    pub commit: String,
    /// The repository's `HEAD` and git index when computed, see
    /// [`helix_vcs::DiffProviderRegistry::repo_state`].
    pub repo_state: String,
    /// The paths whose content the files' state depends on, with their size and modification
    /// time then.
    pub watched: Vec<(PathBuf, Option<(u64, std::time::SystemTime)>)>,
    pub files: Vec<NavFile>,
}

impl ReviewNav {
    /// Whether everything changed since the baseline is committed: every file matches `HEAD` and
    /// the git index, and none has unsaved changes in `editor`.
    pub fn all_committed(&self, editor: &crate::Editor) -> bool {
        self.files.iter().all(|file| {
            file.in_head
                && file.in_index
                && !editor
                    .document_by_path(&file.path)
                    .is_some_and(crate::Document::is_modified)
        })
    }
}

/// A file changed since the review baseline.
#[derive(Debug, Clone)]
pub struct NavFile {
    pub path: PathBuf,
    pub deleted: bool,
    /// Whether the file is new since the baseline.
    pub added: bool,
    /// Whether the file on disk matches `HEAD` (a deleted one: `HEAD` has none either).
    pub in_head: bool,
    /// Whether the file on disk matches the git index.
    pub in_index: bool,
}

/// The GitHub pull request under review (`:review-pr`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// The base repository as `owner/name`.
    pub repo: String,
    pub head_oid: String,
    pub base_ref: String,
    /// The merge base of the base branch and the pull request, the review baseline.
    pub merge_base: String,
    /// The branch (or detached commit) checked out before the pull request, checked out again
    /// when its review ends.
    pub previous: String,
    /// The directory review batches are handed to gtc in, if gtc opened the pull request.
    pub batch_dir: Option<PathBuf>,
}

/// The pi hook's ownership ledger, `<workspace>/.pi/helix-review/state/ownership.json`: who
/// changed which uncommitted lines of each file.
#[derive(Debug, Default, Deserialize)]
pub struct Ownership {
    pub files: HashMap<PathBuf, OwnedFile>,
}

/// The owned ranges of one file, valid for the content with blob id `blob`.
#[derive(Debug, Deserialize)]
pub struct OwnedFile {
    pub blob: String,
    pub ranges: Vec<OwnedRange>,
}

/// 1-based lines `start..end` (lines removed just before `start` when equal) last changed by
/// `owner`: a pi session label, or `user`.
#[derive(Debug, Deserialize)]
pub struct OwnedRange {
    pub start: usize,
    pub end: usize,
    pub owner: String,
}

impl Ownership {
    /// The ledger of the workspace, empty when it is missing or unreadable.
    pub fn load(workspace: &Path) -> Self {
        std::fs::read(workspace.join(".pi/helix-review/state/ownership.json"))
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default()
    }
}

impl OwnedFile {
    /// The owners of its ranges, in order, `you` for the user.
    pub fn owner_names(&self) -> Vec<&str> {
        let mut names = Vec::new();
        for range in &self.ranges {
            if !names.contains(&range.name()) {
                names.push(range.name());
            }
        }
        names
    }
}

impl OwnedRange {
    /// The name shown for the owner: the session label, or `you`.
    pub fn name(&self) -> &str {
        match self.owner.as_str() {
            "user" => "you",
            owner => owner,
        }
    }
}

/// How far a review hunk has got towards a commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkStage {
    Unstaged,
    /// The git index holds it, `HEAD` does not.
    Staged,
    /// `HEAD` holds it: it was committed after the review baseline.
    Committed,
}

/// Exact content and coordinates of a reviewed change within one immutable baseline.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReviewedHunk {
    before: ops::Range<usize>,
    after: ops::Range<usize>,
    text: String,
}

/// A review of a document against an immutable Git snapshot.
#[derive(Debug)]
pub struct Review {
    snapshot: Rope,
    baseline: ReviewBaseline,
    hunks: Vec<Hunk>,
    blocks: Vec<OldBlock>,
    focus: HashMap<ViewId, OldFocus>,
    suspended: bool,
    /// The git index content last given to [`Self::set_index`], in its worktree form.
    index: Option<Vec<u8>>,
    /// Per hunk, whether `index` already holds it; recomputed on every edit.
    staged: Vec<bool>,
    /// The `HEAD` content last given to [`Self::set_index`], in its worktree form.
    head: Option<Vec<u8>>,
    /// Per hunk, whether `head` already holds it, i.e. it was committed after the baseline.
    committed: Vec<bool>,
    /// The pull request's comment threads on this file, see `:review-pr`.
    threads: Vec<ReviewThread>,
    /// The pending comments on this file with their kind (`pi`, `pr` or `reply`), oldest first.
    comments: Vec<(&'static str, ReviewComment)>,
    /// The owners of each block, by its first 0-based document line, until the next edit.
    owners: Vec<(usize, String)>,
    hunk_ids: Vec<ReviewedHunk>,
    reviewed: HashSet<ReviewedHunk>,
    deleted_file: bool,
}

impl Review {
    pub const MAX_BYTES: usize = 256 * 1024;
    pub const MAX_LINES: usize = 10_000;
    pub const LIMIT_MESSAGE: &str = "review limit exceeded (256 KiB or 10,000 lines per side); reduce the file, then use :review or :review-refresh";

    fn compute_hunks(snapshot: &Rope, doc: &Rope) -> Option<Vec<Hunk>> {
        if [snapshot, doc]
            .iter()
            .any(|text| text.len_bytes() > Self::MAX_BYTES || text.len_lines() > Self::MAX_LINES)
        {
            return None;
        }
        compute_hunks(snapshot.clone(), doc.clone())
    }

    pub fn new(snapshot: Rope, baseline: ReviewBaseline, doc: &Rope) -> Result<Self> {
        let hunks = Self::compute_hunks(&snapshot, doc).context(Self::LIMIT_MESSAGE)?;
        let mut review = Review {
            snapshot,
            baseline,
            hunks,
            blocks: Vec::new(),
            focus: HashMap::new(),
            suspended: false,
            index: None,
            staged: Vec::new(),
            head: None,
            committed: Vec::new(),
            threads: Vec::new(),
            comments: Vec::new(),
            owners: Vec::new(),
            hunk_ids: Vec::new(),
            reviewed: HashSet::new(),
            deleted_file: false,
        };
        review.update_reviewed(doc);
        review.update_blocks();
        Ok(review)
    }

    pub fn is_suspended(&self) -> bool {
        self.suspended
    }

    pub fn update(&mut self, doc: &Rope) {
        if self.suspended {
            return;
        }
        let Some(hunks) = Self::compute_hunks(&self.snapshot, doc) else {
            self.suspended = true;
            self.staged.clear();
            self.committed.clear();
            self.hunks.clear();
            self.blocks.clear();
            self.focus.clear();
            self.hunk_ids.clear();
            self.reviewed.clear();
            return;
        };
        self.hunks = hunks;
        self.update_reviewed(doc);
        // The ledger's lines no longer match the document.
        self.owners.clear();
        self.update_staged(doc);
        self.update_blocks();
        self.retain_focus();
    }

    fn retain_focus(&mut self) {
        // Focus is kept in snapshot coordinates and stays valid while its old lines are
        // still removed from the document.
        let focus = std::mem::take(&mut self.focus);
        let text = self.snapshot.slice(..);
        self.focus = focus
            .into_iter()
            .filter(|(_, focus)| {
                self.block_index_at(focus.range.cursor(text))
                    .map(|idx| self.block_chars(&self.blocks[idx]))
                    .is_some_and(|span| {
                        span.start <= focus.range.from() && focus.range.to() <= span.end
                    })
            })
            .collect();
    }

    fn update_reviewed(&mut self, doc: &Rope) {
        self.hunk_ids = self
            .hunks
            .iter()
            .map(|hunk| {
                let before = hunk.before.start as usize..hunk.before.end as usize;
                let after = hunk.after.start as usize..hunk.after.end as usize;
                let text = doc
                    .slice(doc.line_to_char(after.start)..doc.line_to_char(after.end))
                    .to_string();
                ReviewedHunk {
                    before,
                    after,
                    text,
                }
            })
            .collect();
        let current: HashSet<_> = self.hunk_ids.iter().collect();
        self.reviewed.retain(|identity| current.contains(identity));
    }

    /// The current change: old focus wins; otherwise only the primary cursor is considered.
    pub fn hunk_for_cursor(&self, view: ViewId, line: usize) -> Option<usize> {
        if let Some(block) = self.focused_block_index(view).map(|idx| &self.blocks[idx]) {
            return self.hunks.iter().position(|hunk| {
                hunk.before.start as usize == block.old.start
                    && hunk.after.start as usize == block.current.start
                    && hunk.after.end as usize == block.current.end
            });
        }
        let hunk = helix_vcs::find_hunk(&self.hunks, &(line..line + 1))?;
        self.hunks
            .iter()
            .position(|candidate| std::ptr::eq(candidate, hunk))
    }

    pub fn hunk_stage(&self, index: usize) -> HunkStage {
        self.stage(index)
    }

    pub fn hunk_reviewed(&self, index: usize) -> bool {
        self.hunk_ids
            .get(index)
            .is_some_and(|identity| self.reviewed.contains(identity))
    }

    /// Sets local reading progress; this never changes the git index.
    pub fn set_hunk_reviewed(&mut self, index: usize, reviewed: bool) {
        let Some(identity) = self.hunk_ids.get(index) else {
            return;
        };
        if reviewed {
            self.reviewed.insert(identity.clone());
        } else {
            self.reviewed.remove(identity);
        }
    }

    /// Eligible changes in order, with their first document line and local reading progress.
    pub fn review_changes(
        &self,
        include_uncommitted: bool,
    ) -> impl Iterator<Item = (usize, usize, bool)> + '_ {
        self.hunks
            .iter()
            .enumerate()
            .filter(move |(idx, _)| include_uncommitted || self.is_committed(*idx))
            .map(|(idx, hunk)| (idx, hunk.after.start as usize, self.hunk_reviewed(idx)))
    }

    pub fn reviewed_hunks(&self) -> usize {
        self.reviewed.len()
    }

    /// Whether commands opened this as an empty, read-only view of a deleted file.
    pub fn is_deleted_file(&self) -> bool {
        self.deleted_file
    }

    pub fn set_deleted_file(&mut self, deleted: bool) {
        self.deleted_file = deleted;
    }

    fn update_blocks(&mut self) {
        let text = self.snapshot.slice(..);
        self.blocks = self
            .hunks
            .iter()
            .map(OldBlock::new)
            .map(|mut block| {
                if block.old.end == text.len_lines()
                    && text.line_to_char(block.old.end - 1) == text.len_chars()
                {
                    block.old.end -= 1;
                }
                block
            })
            .filter(|block| block.old.start < block.old.end)
            .collect();
        self.blocks.sort_by_key(|block| block.anchor_line);
    }

    /// The historical content the document is compared against.
    pub fn snapshot(&self) -> &Rope {
        &self.snapshot
    }

    pub fn baseline(&self) -> &ReviewBaseline {
        &self.baseline
    }

    /// The status shown when the review starts, including retained local reading progress.
    pub fn summary(&self) -> String {
        let mut summary = self.change_summary();
        if !self.reviewed.is_empty() {
            summary.push_str(&format!(
                "; {}/{} changes reviewed",
                self.reviewed.len(),
                self.hunks.len()
            ));
        }
        summary
    }

    fn change_summary(&self) -> String {
        if self.hunks.is_empty() {
            return format!("Reviewing against the {}: no changes", self.baseline);
        }
        let plural = |count: usize, noun: &str| match count {
            1 => format!("1 {noun}"),
            _ => format!("{count} {noun}s"),
        };
        if self.blocks.is_empty() {
            return format!(
                "Reviewing against the {}: {}, no old text",
                self.baseline,
                plural(self.hunks.len(), "added hunk")
            );
        }
        format!(
            "Reviewing against the {}: {} of old text, :review-old to inspect them",
            self.baseline,
            plural(self.blocks.len(), "block")
        )
    }

    /// All old blocks, ordered by their position in both the snapshot and the document.
    pub fn blocks(&self) -> &[OldBlock] {
        &self.blocks
    }

    /// The old blocks shown below document line `line`.
    pub fn blocks_at(&self, line: usize) -> &[OldBlock] {
        let start = self
            .blocks
            .partition_point(|block| block.anchor_line < line);
        let len = self.blocks[start..].partition_point(|block| block.anchor_line == line);
        &self.blocks[start..start + len]
    }

    /// The number of hunks, each a run of changed lines.
    pub fn hunk_count(&self) -> usize {
        self.hunks.len()
    }

    /// How many hunks the git index holds but `HEAD` does not, as given to [`Self::set_index`].
    pub fn staged_hunks(&self) -> usize {
        (0..self.hunks.len())
            .filter(|&hunk| self.is_staged(hunk) && !self.is_committed(hunk))
            .count()
    }

    /// How many hunks `HEAD` already holds: committed after the baseline.
    pub fn committed_hunks(&self) -> usize {
        self.committed
            .iter()
            .filter(|&&committed| committed)
            .count()
    }

    fn is_staged(&self, hunk: usize) -> bool {
        self.staged.get(hunk).copied().unwrap_or(false)
    }

    fn is_committed(&self, hunk: usize) -> bool {
        self.committed.get(hunk).copied().unwrap_or(false)
    }

    /// Records the git index content `index` and the `HEAD` content `head` (in their worktree
    /// forms) and marks the hunks each already holds in full for the document text `doc`. Edits
    /// keep the marks up to date against them.
    pub fn set_index(&mut self, index: &[u8], head: Option<&[u8]>, doc: &Rope) {
        self.index = Some(index.to_vec());
        self.head = head.map(<[u8]>::to_vec);
        self.update_staged(doc);
    }

    fn update_staged(&mut self, doc: &Rope) {
        let doc = doc.to_string();
        let held = |base: &Option<Vec<u8>>| -> Vec<bool> {
            let Some(base) = base else {
                return Vec::new();
            };
            let rest = helix_vcs::line_hunks(base, doc.as_bytes());
            self.hunks
                .iter()
                .map(|hunk| {
                    let lines = hunk.after.start as usize..hunk.after.end as usize;
                    helix_vcs::find_hunk(&rest, &lines).is_none()
                })
                .collect()
        };
        self.staged = held(&self.index);
        self.committed = held(&self.head);
    }

    fn stage(&self, hunk: usize) -> HunkStage {
        if self.is_committed(hunk) {
            HunkStage::Committed
        } else if self.is_staged(hunk) {
            HunkStage::Staged
        } else {
            HunkStage::Unstaged
        }
    }

    /// Whether `HEAD` or the git index already holds the hunk `block` comes from.
    pub fn block_stage(&self, block: &OldBlock) -> HunkStage {
        self.hunks
            .iter()
            .position(|hunk| {
                let current = hunk.after.start as usize..hunk.after.end as usize;
                hunk.before.start as usize == block.old.start && current == block.current
            })
            .map_or(HunkStage::Unstaged, |hunk| self.stage(hunk))
    }

    /// Shows `threads`, the pull request's comment threads on this file.
    pub fn set_threads(&mut self, threads: Vec<ReviewThread>) {
        self.threads = threads;
    }

    /// Keeps threads, pending comments and valid old selections when replacing the same baseline.
    pub fn keep_threads(&mut self, previous: Review) {
        if self.baseline == previous.baseline && self.snapshot == previous.snapshot {
            self.focus = previous.focus;
            self.retain_focus();
            self.reviewed = previous.reviewed;
            let current: HashSet<_> = self.hunk_ids.iter().collect();
            self.reviewed.retain(|identity| current.contains(identity));
        }
        self.threads = previous.threads;
        self.comments = previous.comments;
    }

    /// Shows `comments`, the pending comments on this file with their kind, oldest first.
    pub fn set_comments(&mut self, comments: Vec<(&'static str, ReviewComment)>) {
        self.comments = comments;
    }

    pub fn threads(&self) -> &[ReviewThread] {
        &self.threads
    }

    /// Names the owners of each block from `file`, the ledger's entry for this document.
    pub fn set_owners(&mut self, file: Option<&OwnedFile>) {
        self.owners = file
            .map(|file| {
                self.hunks
                    .iter()
                    .filter_map(|hunk| {
                        let lines = hunk.after.start as usize..hunk.after.end as usize;
                        let mut names = Vec::new();
                        for range in &file.ranges {
                            let (start, end) = (range.start.max(1) - 1, range.end.max(1) - 1);
                            let meets = if start == end {
                                lines.start <= start && start <= lines.end
                            } else {
                                start < lines.end && lines.start < end
                            };
                            if meets && !names.contains(&range.name()) {
                                names.push(range.name());
                            }
                        }
                        (!names.is_empty()).then(|| (lines.start, names.join(" ")))
                    })
                    .collect()
            })
            .unwrap_or_default();
    }

    /// The owners shown on 0-based document line `line`, the first line of a block.
    pub fn owners_at(&self, line: usize) -> Option<&str> {
        self.owners
            .iter()
            .find(|(start, _)| *start == line)
            .map(|(_, names)| names.as_str())
    }

    /// The 0-based document line `thread` is anchored at in a document of `len_lines` lines, and
    /// a tag for threads not on the current text: outdated ones at their original line, and
    /// base-side ones at the first current line of the change that removed their line.
    pub fn thread_anchor(
        &self,
        thread: &ReviewThread,
        len_lines: usize,
    ) -> (usize, Option<&'static str>) {
        let (line, tag) = match (thread.side, thread.line) {
            (ThreadSide::Right, Some(line)) => (line, None),
            (ThreadSide::Right, None) => (thread.original_line.unwrap_or(1), Some("outdated")),
            (ThreadSide::Left, line) => {
                let base = line.or(thread.original_line).unwrap_or(1) - 1;
                let mapped = match self.hunks.iter().find(|hunk| {
                    (hunk.before.start as usize..hunk.before.end as usize).contains(&base)
                }) {
                    Some(hunk) => hunk.after.start as usize,
                    None => {
                        let shift: isize = self
                            .hunks
                            .iter()
                            .filter(|hunk| hunk.before.end as usize <= base)
                            .map(|hunk| hunk.after.len() as isize - hunk.before.len() as isize)
                            .sum();
                        (base as isize + shift).max(0) as usize
                    }
                };
                (mapped + 1, Some("old side"))
            }
        };
        ((line.max(1) - 1).min(len_lines.saturating_sub(1)), tag)
    }

    /// The document line a thread anchored at `anchor` is shown below: the line above it, so the
    /// thread reads above its line, except for the first line.
    fn thread_row_line(anchor: usize) -> usize {
        anchor.saturating_sub(1)
    }

    /// The rows of the threads shown below document line `line` at text width `width`.
    pub fn thread_rows_below(&self, line: usize, len_lines: usize, width: usize) -> Vec<String> {
        let mut threads: Vec<_> = self
            .threads
            .iter()
            .map(|thread| (self.thread_anchor(thread, len_lines), thread))
            .filter(|((anchor, _), _)| Self::thread_row_line(*anchor) == line)
            .collect();
        threads.sort_by_key(|((anchor, _), _)| *anchor);
        threads
            .into_iter()
            .flat_map(|((_, tag), thread)| thread.rows(width, tag))
            .collect()
    }

    /// The document lines threads are shown below, ascending and without repeats.
    pub fn thread_lines(&self, len_lines: usize) -> Vec<usize> {
        let mut lines: Vec<_> = (self.threads.iter())
            .map(|thread| Self::thread_row_line(self.thread_anchor(thread, len_lines).0))
            .collect();
        lines.sort_unstable();
        lines.dedup();
        lines
    }

    /// The block whose old lines `comment` is on, while they are shown.
    fn comment_block(&self, comment: &ReviewComment) -> Option<&OldBlock> {
        if comment.side != ReviewSide::Old || comment.baseline != self.baseline {
            return None;
        }
        let line = comment.end_line.max(1) - 1;
        self.blocks.iter().find(|block| block.old.contains(&line))
    }

    /// The document line `comment` is shown below unless it is under old lines: the last line it
    /// covers, or for old text the line it is shown at.
    fn comment_line(comment: &ReviewComment, len_lines: usize) -> usize {
        let line = match comment.side {
            ReviewSide::Current => comment.end_line,
            ReviewSide::Old => comment.current_line,
        };
        (line.max(1) - 1).min(len_lines.saturating_sub(1))
    }

    /// The rows of the pending comments shown directly below document line `line` at text width
    /// `width`.
    pub fn comment_rows_below(&self, line: usize, len_lines: usize, width: usize) -> Vec<String> {
        self.comments
            .iter()
            .filter(|(_, comment)| {
                self.comment_block(comment).is_none()
                    && Self::comment_line(comment, len_lines) == line
            })
            .flat_map(|(kind, comment)| comment.rows(kind, width))
            .collect()
    }

    /// The rows of the pending comments on the old lines of `block`, shown below them.
    pub fn block_comment_rows(&self, block: &OldBlock, width: usize) -> Vec<String> {
        self.comments
            .iter()
            .filter(|(_, comment)| {
                self.comment_block(comment)
                    .is_some_and(|other| other.old == block.old)
            })
            .flat_map(|(kind, comment)| comment.rows(kind, width))
            .collect()
    }

    /// The document lines pending comments are shown below.
    pub fn comment_lines(&self, len_lines: usize) -> Vec<usize> {
        (self.comments.iter())
            .map(|(_, comment)| match self.comment_block(comment) {
                Some(block) => block.anchor_line,
                None => Self::comment_line(comment, len_lines),
            })
            .collect()
    }

    /// The first document line of every unstaged hunk, removals included, in order.
    pub fn unstaged_hunk_lines(&self) -> Vec<usize> {
        (0..self.hunks.len())
            .filter(|&idx| self.stage(idx) == HunkStage::Unstaged)
            .map(|idx| self.hunks[idx].after.start as usize)
            .collect()
    }

    /// Like [`Self::changed_lines`], with whether `HEAD` or the git index already holds each range.
    pub fn changed_lines_stage(&self) -> impl Iterator<Item = (ops::Range<usize>, HunkStage)> + '_ {
        self.hunks
            .iter()
            .enumerate()
            .map(|(idx, hunk)| {
                (
                    hunk.after.start as usize..hunk.after.end as usize,
                    self.stage(idx),
                )
            })
            .filter(|(lines, _)| !lines.is_empty())
    }

    /// The baseline and document line ranges of the hunk whose document lines contain `line`.
    pub fn hunk_at_line(&self, line: usize) -> Option<(ops::Range<usize>, ops::Range<usize>)> {
        self.hunks
            .iter()
            .map(|hunk| {
                (
                    hunk.before.start as usize..hunk.before.end as usize,
                    hunk.after.start as usize..hunk.after.end as usize,
                )
            })
            .find(|(_, current)| current.contains(&line))
    }

    /// The document line where the first hunk starts, where `]g` from the top of the file lands.
    pub fn first_change_line(&self) -> Option<usize> {
        self.hunks.first().map(|hunk| hunk.after.start as usize)
    }

    /// Line ranges of the document that differ from the baseline.
    /// The document lines of every hunk not committed after the baseline, in order; a removal is
    /// an empty range where it was.
    pub fn uncommitted_hunk_lines(&self) -> Vec<ops::Range<usize>> {
        (0..self.hunks.len())
            .filter(|&idx| self.stage(idx) != HunkStage::Committed)
            .map(|idx| self.hunks[idx].after.start as usize..self.hunks[idx].after.end as usize)
            .collect()
    }

    /// The document lines of every hunk, committed ones included, in order; a removal is an empty
    /// range where it was.
    pub fn hunk_lines(&self) -> Vec<ops::Range<usize>> {
        self.hunks
            .iter()
            .map(|hunk| hunk.after.start as usize..hunk.after.end as usize)
            .collect()
    }

    pub fn changed_lines(&self) -> impl Iterator<Item = ops::Range<usize>> + '_ {
        self.hunks
            .iter()
            .map(|hunk| hunk.after.start as usize..hunk.after.end as usize)
            .filter(|lines| !lines.is_empty())
    }

    /// The char range of `block` in the snapshot.
    pub fn block_chars(&self, block: &OldBlock) -> ops::Range<usize> {
        self.snapshot.line_to_char(block.old.start)..self.snapshot.line_to_char(block.old.end)
    }

    fn block_index_at(&self, char_idx: usize) -> Option<usize> {
        let idx = self
            .blocks
            .partition_point(|block| self.block_chars(block).end <= char_idx);
        let block = self.blocks.get(idx)?;
        self.block_chars(block).contains(&char_idx).then_some(idx)
    }

    /// Snapshot line `line` without its line ending.
    pub fn old_line(&self, line: usize) -> RopeSlice<'_> {
        let text = self.snapshot.slice(..);
        text.slice(text.line_to_char(line)..line_end_char_index(&text, line))
    }

    /// The number of visual rows old line `line` occupies.
    pub fn old_line_height(&self, line: usize, text_fmt: &TextFormat) -> usize {
        if !text_fmt.soft_wrap {
            return 1;
        }
        softwrapped_dimensions(self.old_line(line), text_fmt).0
    }

    /// The number of virtual rows `block` occupies.
    pub fn block_height(&self, block: &OldBlock, text_fmt: &TextFormat) -> usize {
        block.is_displaced() as usize
            + block
                .old
                .clone()
                .map(|line| self.old_line_height(line, text_fmt))
                .sum::<usize>()
    }

    /// The first virtual row, counting its label, of block `index` below its anchor line in a
    /// document of `len_lines` lines.
    pub fn block_row(&self, index: usize, len_lines: usize, text_fmt: &TextFormat) -> usize {
        let block = &self.blocks[index];
        let width = text_fmt.viewport_width as usize;
        self.comment_rows_below(block.anchor_line, len_lines, width)
            .len()
            + self
                .blocks_at(block.anchor_line)
                .iter()
                .take_while(|other| other.old.start < block.old.start)
                .map(|other| {
                    self.block_height(other, text_fmt) + self.block_comment_rows(other, width).len()
                })
                .sum::<usize>()
    }

    pub fn focus(&self, view_id: ViewId) -> Option<OldFocus> {
        self.focus.get(&view_id).copied()
    }

    /// The index into [`Self::blocks`] of the block containing the focused cursor.
    pub fn focused_block_index(&self, view_id: ViewId) -> Option<usize> {
        let focus = self.focus.get(&view_id)?;
        self.block_index_at(focus.range.cursor(self.snapshot.slice(..)))
    }

    /// Focuses the start of block `index` in `view_id`.
    pub fn focus_block(&mut self, view_id: ViewId, index: usize) {
        let start = self.block_chars(&self.blocks[index]).start;
        let range = Range::point(start).min_width_1(self.snapshot.slice(..));
        self.focus.insert(
            view_id,
            OldFocus {
                range,
                extend: false,
            },
        );
    }

    /// Replaces the focus of `view_id` with the result of `f`, which receives the snapshot, the
    /// current focus and the char range of the focused block. The new selection is clamped to
    /// that block.
    pub fn update_focus(
        &mut self,
        view_id: ViewId,
        f: impl FnOnce(RopeSlice, OldFocus, ops::Range<usize>) -> OldFocus,
    ) {
        let Some(idx) = self.focused_block_index(view_id) else {
            return;
        };
        let span = self.block_chars(&self.blocks[idx]);
        let text = self.snapshot.slice(..);
        let mut focus = f(text, self.focus[&view_id], span.clone());
        focus.range = clamp_range(focus.range, text, span);
        self.focus.insert(view_id, focus);
    }

    /// Leaves the old text in `view_id`. Returns whether it was focused.
    pub fn clear_focus(&mut self, view_id: ViewId) -> bool {
        self.focus.remove(&view_id).is_some()
    }

    pub(crate) fn remove_view(&mut self, view_id: ViewId) {
        self.focus.remove(&view_id);
    }

    /// Returns the document line below which the focused old text of `view_id` is shown, and
    /// the position of the focused cursor in the virtual rows below that line.
    pub fn focus_position(
        &self,
        view_id: ViewId,
        len_lines: usize,
        text_fmt: &TextFormat,
    ) -> Option<(usize, Position)> {
        let idx = self.focused_block_index(view_id)?;
        let block = &self.blocks[idx];
        let text = self.snapshot.slice(..);
        let cursor = self.focus[&view_id].range.cursor(text);
        let line = text.char_to_line(cursor);
        let mut row = self.block_row(idx, len_lines, text_fmt);
        row += block.is_displaced() as usize;
        row += (block.old.start..line)
            .map(|line| self.old_line_height(line, text_fmt))
            .sum::<usize>();
        let pos = visual_offset_from_block(
            self.old_line(line),
            0,
            cursor - text.line_to_char(line),
            text_fmt,
            &TextAnnotations::default(),
        )
        .0;
        Some((block.anchor_line, Position::new(row + pos.row, pos.col)))
    }
}

#[cfg(test)]
mod reviewed_tests {
    use super::*;

    fn baseline(commit: char) -> ReviewBaseline {
        ReviewBaseline::GitCommit {
            commit_oid: commit.to_string().repeat(40),
            blob_oid: Some("blob".into()),
        }
    }

    #[test]
    fn reviewed_changes_invalidate_only_changed_hunks_and_never_return_on_undo() {
        let before = Rope::from("a\nb\nc\nd\ne\nf\n");
        let after = Rope::from("a\nB\nc\nd\ne\nF\n");
        let mut review = Review::new(before, baseline('a'), &after).unwrap();
        assert_eq!(review.hunk_count(), 2);
        review.set_hunk_reviewed(0, true);
        review.set_hunk_reviewed(1, true);
        review.update(&Rope::from("a\nBB\nc\nd\ne\nF\n"));
        assert!(!review.hunk_reviewed(0));
        assert!(review.hunk_reviewed(1));
        review.update(&after);
        assert!(!review.hunk_reviewed(0));
        assert!(review.hunk_reviewed(1));
        review.set_hunk_reviewed(1, false);
        assert_eq!(review.reviewed_hunks(), 0);
    }

    #[test]
    fn reviewed_changes_survive_same_review_refresh_but_not_rebased_content() {
        let before = Rope::from("before\n");
        let after = Rope::from("after\n");
        let mut previous = Review::new(before.clone(), baseline('a'), &after).unwrap();
        previous.set_hunk_reviewed(0, true);
        let mut refreshed = Review::new(before.clone(), baseline('a'), &after).unwrap();
        refreshed.keep_threads(previous);
        assert!(refreshed.hunk_reviewed(0));
        let mut rebased = Review::new(before, baseline('b'), &after).unwrap();
        rebased.keep_threads(refreshed);
        assert_eq!(rebased.reviewed_hunks(), 0);
    }

    #[test]
    fn reviewed_deletion_uses_focused_old_block_without_changing_git_stage() {
        let before = Rope::from("keep\ngone\nend\n");
        let after = Rope::from("keep\nend\n");
        let mut review = Review::new(before, baseline('a'), &after).unwrap();
        review.set_index(b"keep\nend\n", Some(b"keep\nend\n"), &after);
        let view = ViewId::default();
        review.focus_block(view, 0);
        let current = review.hunk_for_cursor(view, 999).unwrap();
        assert_eq!(current, 0);
        assert_eq!(review.hunk_stage(current), HunkStage::Committed);
        review.set_hunk_reviewed(current, true);
        assert!(review.hunk_reviewed(current));
        assert_eq!(review.hunk_stage(current), HunkStage::Committed);
        assert_eq!(review.snapshot(), "keep\ngone\nend\n");
    }
}

fn clamp_range(range: Range, text: RopeSlice, span: ops::Range<usize>) -> Range {
    let clamped = Range {
        anchor: range.anchor.clamp(span.start, span.end),
        head: range.head.clamp(span.start, span.end),
        old_visual_position: range.old_visual_position,
    };
    match clamped.anchor == clamped.head {
        true if clamped.head == span.end => {
            Range::new(prev_grapheme_boundary(text, span.end), span.end)
        }
        true => clamped.min_width_1(text),
        false => clamped,
    }
}

/// Reserves virtual rows for the old lines shown below document lines.
pub(crate) struct ReviewLines<'a> {
    review: &'a Review,
    text: RopeSlice<'a>,
    text_fmt: TextFormat,
}

impl<'a> ReviewLines<'a> {
    #[allow(clippy::new_ret_no_self)]
    pub(crate) fn new(
        review: &'a Review,
        text: RopeSlice<'a>,
        text_fmt: TextFormat,
    ) -> Box<dyn LineAnnotation + 'a> {
        Box::new(ReviewLines {
            review,
            text,
            text_fmt,
        })
    }
}

impl LineAnnotation for ReviewLines<'_> {
    fn insert_virtual_lines(
        &mut self,
        line_end_char_idx: usize,
        _line_end_visual_pos: Position,
        doc_line: usize,
    ) -> Position {
        // This is also called at soft wrap points but old lines only follow a full line.
        if doc_line + 1 >= self.text.len_lines()
            || line_end_char_idx != self.text.line_to_char(doc_line + 1)
        {
            return Position::new(0, 0);
        }
        let (len_lines, width) = (self.text.len_lines(), self.text_fmt.viewport_width as usize);
        let rows: usize = self
            .review
            .blocks_at(doc_line)
            .iter()
            .map(|block| {
                self.review.block_height(block, &self.text_fmt)
                    + self.review.block_comment_rows(block, width).len()
            })
            .sum();
        let comments = (self.review)
            .comment_rows_below(doc_line, len_lines, width)
            .len();
        let threads = (self.review)
            .thread_rows_below(doc_line, len_lines, width)
            .len();
        Position::new(rows + comments + threads, 0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewSide {
    Old,
    Current,
}

const CONTEXT_LINES: usize = 3;

/// A review comment, serialized as described in `contrib/pi-helix-review/schema.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewComment {
    pub path: PathBuf,
    pub side: ReviewSide,
    pub baseline: ReviewBaseline,
    pub start_line: usize,
    pub end_line: usize,
    pub current_line: usize,
    pub selected_text: String,
    pub context_before: String,
    pub context_after: String,
    /// The smallest function, type or similar item around the selection, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enclosing_text: Option<String>,
    pub comment: String,
    /// The pull request comment thread (its first comment's id) this comment replies to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<u64>,
    /// When the comment was made, ordering it among the comments of other kinds.
    #[serde(skip)]
    pub created: Instant,
}

impl ReviewComment {
    /// Creates a comment on `range` of `text`, the snapshot of `side`. `current_line` is the
    /// 0-based document line the selection is shown at.
    pub fn new(
        path: PathBuf,
        side: ReviewSide,
        baseline: &ReviewBaseline,
        text: RopeSlice,
        range: Range,
        current_line: usize,
        comment: String,
    ) -> Self {
        let (from, to) = (range.from(), range.to());
        let first_line = text.char_to_line(from);
        let last_line = text.char_to_line(to.saturating_sub(1).max(from));
        let context_start = text.line_to_char(first_line.saturating_sub(CONTEXT_LINES));
        let context_end = text.line_to_char((last_line + CONTEXT_LINES + 1).min(text.len_lines()));
        ReviewComment {
            path,
            side,
            baseline: baseline.clone(),
            start_line: first_line + 1,
            end_line: last_line + 1,
            current_line: current_line + 1,
            selected_text: text.slice(from..to).to_string(),
            context_before: text.slice(context_start..from).to_string(),
            context_after: text.slice(to..context_end).to_string(),
            enclosing_text: None,
            comment,
            reply_to: None,
            created: Instant::now(),
        }
    }

    pub const MAX_ROWS: usize = 8;

    /// The rows of the comment of `kind` shown below its lines at text width `width`:
    /// `▲ kind │ text`, wrapped, with continuation rows under the `│`, cut at
    /// [`Self::MAX_ROWS`] rows.
    pub fn rows(&self, kind: &str, width: usize) -> Vec<String> {
        let first = format!("▲ {kind} │ ");
        let next = format!("{}│ ", " ".repeat(first.chars().count() - 2));
        let text_width = width.saturating_sub(first.chars().count()).max(1);
        let mut rows: Vec<_> = (self.comment.replace('\t', "    ").lines())
            .flat_map(|line| wrap_columns(line, text_width))
            .enumerate()
            .map(|(row, text)| format!("{}{text}", if row == 0 { &first } else { &next }))
            .collect();
        if rows.is_empty() {
            rows.push(first);
        }
        if rows.len() > Self::MAX_ROWS {
            rows.truncate(Self::MAX_ROWS - 1);
            rows.push(format!("{next}…"));
        }
        rows
    }
}

/// The side of a pull request diff a comment thread is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadSide {
    /// The base (removed) side.
    Left,
    /// The head (current) side.
    Right,
}

/// One comment of a pull request comment thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadComment {
    pub id: u64,
    pub login: String,
    pub body: String,
}

/// A pull request review comment thread on a file, shown above the line it is anchored at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewThread {
    pub path: PathBuf,
    pub side: ThreadSide,
    /// The 1-based line on `side` in the pull request's latest diff, `None` when outdated.
    pub line: Option<usize>,
    /// The 1-based line on `side` when the thread was started.
    pub original_line: Option<usize>,
    /// The root comment first, then the replies, oldest first.
    pub comments: Vec<ThreadComment>,
}

impl ReviewThread {
    pub const MAX_ROWS: usize = 12;

    pub fn root_id(&self) -> u64 {
        self.comments.first().map_or(0, |comment| comment.id)
    }

    /// The rows of the thread at text width `width`: each comment as `login │ text`, wrapped, with
    /// continuation rows under the `│`, replies indented, cut at [`Self::MAX_ROWS`] rows. `tag`
    /// is appended to the first row.
    pub fn rows(&self, width: usize, tag: Option<&str>) -> Vec<String> {
        let mut rows = Vec::new();
        for (idx, comment) in self.comments.iter().enumerate() {
            let indent = if idx == 0 { "" } else { "  " };
            let first = format!("{indent}{} │ ", comment.login);
            let next = format!("{}│ ", " ".repeat(first.chars().count() - 2));
            let text_width = width.saturating_sub(first.chars().count()).max(1);
            let mut body = comment.body.replace('\t', "    ");
            if let (0, Some(tag)) = (idx, tag) {
                body = match body.split_once('\n') {
                    Some((head, rest)) => format!("{head} ({tag})\n{rest}"),
                    None => format!("{body} ({tag})"),
                };
            }
            let lines: Vec<_> = body
                .lines()
                .flat_map(|line| wrap_columns(line, text_width))
                .collect();
            for (row, text) in lines.iter().enumerate() {
                let lead = if row == 0 { &first } else { &next };
                rows.push(format!("{lead}{text}"));
            }
            if lines.is_empty() {
                rows.push(first);
            }
        }
        if rows.len() > Self::MAX_ROWS {
            rows.truncate(Self::MAX_ROWS - 1);
            rows.push("      │ …".to_owned());
        }
        rows
    }
}

/// Splits `line` into pieces at most `width` columns wide; an empty line gives one empty piece.
fn wrap_columns(line: &str, width: usize) -> Vec<String> {
    use helix_core::unicode::width::UnicodeWidthChar;
    let mut rows = vec![String::new()];
    let mut columns = 0;
    for ch in line.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if columns + ch_width > width && columns > 0 {
            rows.push(String::new());
            columns = 0;
        }
        rows.last_mut().unwrap().push(ch);
        columns += ch_width;
    }
    rows
}

/// What the reviewer asks the agent to do with a handoff, beyond the edits and comments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ReviewRequest {
    /// Commit what is staged in the git index, with an optional subject.
    CommitStaged {
        #[serde(skip_serializing_if = "Option::is_none")]
        subject: Option<String>,
    },
    /// A free-text instruction.
    Message { text: String },
    /// Do what the batch's comments ask (`:review-ask`).
    Ask,
    /// Publish the comments and saved edits as a pull request review, with an optional summary.
    PrReview {
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
}

/// Pending review comments and reviewed files, exported together as one batch.
#[derive(Debug, Default)]
pub struct ReviewBatch {
    files: BTreeMap<PathBuf, ReviewBaseline>,
    comments: Vec<ReviewComment>,
    request: Option<ReviewRequest>,
    target: Option<String>,
}

impl ReviewBatch {
    /// Records the latest baseline reviewed for `path`.
    pub fn add_file(&mut self, path: PathBuf, baseline: ReviewBaseline) {
        self.files.insert(path, baseline);
    }

    /// Adds a pending comment and returns the number of pending comments.
    pub fn add_comment(&mut self, comment: ReviewComment) -> usize {
        self.files
            .entry(comment.path.clone())
            .or_insert_with(|| comment.baseline.clone());
        self.comments.push(comment);
        self.comments.len()
    }

    pub fn files(&self) -> impl Iterator<Item = &Path> {
        self.files.keys().map(PathBuf::as_path)
    }

    pub fn comments(&self) -> &[ReviewComment] {
        &self.comments
    }

    /// Removes up to `count` of the most recently added comments and returns how many it removed.
    pub fn remove_latest_comments(&mut self, count: usize) -> usize {
        let removed = count.min(self.comments.len());
        self.comments.truncate(self.comments.len() - removed);
        removed
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.comments.is_empty() && self.request.is_none()
    }

    /// Adds the files and comments of the later batch `other`, keeping comments in the order they
    /// were made; its request and target replace this batch's.
    pub fn absorb(&mut self, other: ReviewBatch) {
        for (path, baseline) in other.files {
            self.files.entry(path).or_insert(baseline);
        }
        self.comments.extend(other.comments);
        self.comments.sort_by_key(|comment| comment.created);
        if let Some(request) = other.request {
            self.request = Some(request);
        }
        if let Some(target) = other.target {
            self.target = Some(target);
        }
    }

    /// Sets the request the next export carries, replacing any earlier one.
    pub fn set_request(&mut self, request: ReviewRequest) {
        self.request = Some(request);
    }

    /// Addresses the next export to the pi session `target` (a label such as `pi-2`).
    pub fn set_target(&mut self, target: String) {
        self.target = Some(target);
    }

    /// The pi session the next export is addressed to, if any.
    pub fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }

    /// Writes accepted files and comments as JSON, limiting asks to commented paths.
    /// Exported entries are removed; excluded entries and failed writes remain pending.
    pub fn export(&mut self, path: &Path, include_file: impl Fn(&Path) -> bool) -> Result<()> {
        let comment_paths = matches!(self.request, Some(ReviewRequest::Ask)).then(|| {
            self.comments
                .iter()
                .map(|comment| comment.path.clone())
                .collect::<HashSet<_>>()
        });
        let include = |file: &Path| {
            include_file(file)
                && comment_paths
                    .as_ref()
                    .is_none_or(|paths| paths.contains(file))
        };
        #[derive(Serialize)]
        struct File<'a> {
            path: &'a Path,
            baseline: &'a ReviewBaseline,
        }
        #[derive(Serialize)]
        struct Batch<'a> {
            format: &'static str,
            version: u32,
            files: Vec<File<'a>>,
            comments: Vec<&'a ReviewComment>,
            #[serde(skip_serializing_if = "Option::is_none")]
            request: Option<&'a ReviewRequest>,
            #[serde(skip_serializing_if = "Option::is_none")]
            target: Option<&'a str>,
        }

        let batch = Batch {
            format: "helix-review-batch",
            version: 2,
            files: self
                .files
                .iter()
                .filter(|(path, _)| include(path))
                .map(|(path, baseline)| File { path, baseline })
                .collect(),
            comments: self
                .comments
                .iter()
                .filter(|comment| include(&comment.path))
                .collect(),
            request: self.request.as_ref(),
            target: self.target.as_deref(),
        };
        let mut json = serde_json::to_vec_pretty(&batch)?;
        json.push(b'\n');

        // `create_new` refuses existing files, so an export never replaces a source file.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        if let Err(err) = file.write_all(&json).and_then(|()| file.sync_all()) {
            drop(file);
            let _ = std::fs::remove_file(path);
            return Err(anyhow!("failed to write {}: {err}", path.display()));
        }
        self.files.retain(|path, _| !include(path));
        self.comments.retain(|comment| !include(&comment.path));
        self.request = None;
        self.target = None;
        Ok(())
    }
}
