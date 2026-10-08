//! Inline review of a document against an index or commit snapshot.

use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::io::Write;
use std::ops;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use helix_core::doc_formatter::TextFormat;
use helix_core::graphemes::prev_grapheme_boundary;
use helix_core::line_ending::line_end_char_index;
use helix_core::text_annotations::{LineAnnotation, TextAnnotations};
use helix_core::{
    softwrapped_dimensions, visual_offset_from_block, Position, Range, Rope, RopeSlice,
};
use helix_vcs::{compute_hunks, Hunk};
use serde::Serialize;

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
        };
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
            self.hunks.clear();
            self.blocks.clear();
            self.focus.clear();
            return;
        };
        self.hunks = hunks;
        self.update_staged(doc);
        self.update_blocks();
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

    /// The status shown when the review starts.
    pub fn summary(&self) -> String {
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

    /// How many hunks the git index already holds, as given to [`Self::set_index`].
    pub fn staged_hunks(&self) -> usize {
        self.staged.iter().filter(|&&staged| staged).count()
    }

    fn is_staged(&self, hunk: usize) -> bool {
        self.staged.get(hunk).copied().unwrap_or(false)
    }

    /// Records the git index content `index` (in its worktree form) and marks the hunks it already
    /// holds in full for the document text `doc`. Edits keep the marks up to date against it.
    pub fn set_index(&mut self, index: &[u8], doc: &Rope) {
        self.index = Some(index.to_vec());
        self.update_staged(doc);
    }

    fn update_staged(&mut self, doc: &Rope) {
        let Some(index) = &self.index else {
            self.staged.clear();
            return;
        };
        let unstaged = helix_vcs::line_hunks(index, doc.to_string().as_bytes());
        self.staged = self
            .hunks
            .iter()
            .map(|hunk| {
                let lines = hunk.after.start as usize..hunk.after.end as usize;
                helix_vcs::find_hunk(&unstaged, &lines).is_none()
            })
            .collect();
    }

    /// Whether the git index already holds the hunk `block` comes from.
    pub fn is_block_staged(&self, block: &OldBlock) -> bool {
        self.hunks
            .iter()
            .position(|hunk| {
                let current = hunk.after.start as usize..hunk.after.end as usize;
                hunk.before.start as usize == block.old.start && current == block.current
            })
            .is_some_and(|hunk| self.is_staged(hunk))
    }

    /// Like [`Self::changed_lines`], with whether the git index already holds each range.
    pub fn changed_lines_staged(&self) -> impl Iterator<Item = (ops::Range<usize>, bool)> + '_ {
        self.hunks
            .iter()
            .enumerate()
            .map(|(idx, hunk)| {
                (
                    hunk.after.start as usize..hunk.after.end as usize,
                    self.is_staged(idx),
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

    /// The first virtual row, counting its label, of block `index` below its anchor line.
    pub fn block_row(&self, index: usize, text_fmt: &TextFormat) -> usize {
        let block = &self.blocks[index];
        self.blocks_at(block.anchor_line)
            .iter()
            .take_while(|other| other.old.start < block.old.start)
            .map(|other| self.block_height(other, text_fmt))
            .sum()
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
        text_fmt: &TextFormat,
    ) -> Option<(usize, Position)> {
        let idx = self.focused_block_index(view_id)?;
        let block = &self.blocks[idx];
        let text = self.snapshot.slice(..);
        let cursor = self.focus[&view_id].range.cursor(text);
        let line = text.char_to_line(cursor);
        let mut row = self.block_row(idx, text_fmt);
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
        let rows = self
            .review
            .blocks_at(doc_line)
            .iter()
            .map(|block| self.review.block_height(block, &self.text_fmt))
            .sum();
        Position::new(rows, 0)
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
#[derive(Debug, Clone, Serialize)]
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
    pub comment: String,
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
            comment,
        }
    }
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
}

/// Pending review comments and reviewed files, exported together as one batch.
#[derive(Debug, Default)]
pub struct ReviewBatch {
    files: BTreeMap<PathBuf, ReviewBaseline>,
    comments: Vec<ReviewComment>,
    request: Option<ReviewRequest>,
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

    /// Sets the request the next export carries, replacing any earlier one.
    pub fn set_request(&mut self, request: ReviewRequest) {
        self.request = Some(request);
    }

    /// Writes the files accepted by `include` and their comments as JSON to the new file `path`
    /// and removes them from the batch. Nothing is removed if writing fails.
    pub fn export(&mut self, path: &Path, include: impl Fn(&Path) -> bool) -> Result<()> {
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
        Ok(())
    }
}
