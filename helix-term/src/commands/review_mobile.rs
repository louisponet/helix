//! Local, explicitly accepted PR review drafts for the phone transport.
//!
//! This module only reads Git and saved files. Publication belongs to the host; its
//! acknowledgement removes the captured draft entries without touching local edits.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::Path;

use anyhow::{ensure, Context, Result};
use helix_view::review::{PullRequest, ReviewComment, ReviewSide};
use helix_view::Editor;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::review::selection_comments;

// Leave space for the transport envelope, request UUID and a possible top-level error.
pub(crate) const MAX_SNAPSHOT: usize = 48 * 1024 - 256;
const MAX_FILE: u64 = 1024 * 1024;
const HISTORY: usize = 16;
type Anchor = (&'static str, usize, usize, String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Pr {
    url: String,
    repo: String,
    number: u64,
    head_sha: String,
    title: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Selection {
    id: String,
    path: String,
    side: &'static str,
    start_line: usize,
    line: usize,
    code: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Comment {
    id: String,
    path: String,
    side: &'static str,
    start_line: usize,
    line: usize,
    body: String,
    code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Suggestion {
    id: String,
    path: String,
    start_line: usize,
    line: usize,
    replacement: String,
    original: String,
    accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Snapshot {
    revision: String,
    pr: Pr,
    selection: Option<Selection>,
    comments: Vec<Comment>,
    suggestions: Vec<Suggestion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Default)]
pub(crate) struct Draft {
    key: Option<String>,
    accepted: HashSet<String>,
    snapshots: VecDeque<Snapshot>,
    comment_anchors: HashMap<String, (String, Anchor)>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommentRequest {
    revision: String,
    selection_id: Option<String>,
    id: Option<String>,
    body: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SuggestionRequest {
    revision: String,
    id: String,
    accepted: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompleteRequest {
    revision: String,
}

impl Draft {
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn snapshot(&mut self, editor: &Editor) -> Result<Option<Snapshot>> {
        let Some(pr) = editor.review_pr.as_ref() else {
            self.clear();
            return Ok(None);
        };
        let key = fingerprint(&(
            pr.url.as_str(),
            pr.head_oid.as_str(),
            pr.merge_base.as_str(),
        ))?;
        if self.key.as_ref() != Some(&key) {
            self.clear();
            self.key = Some(key);
        }
        let workspace = helix_loader::find_workspace().0;
        ensure_head(pr, &workspace)?;
        ensure!(
            !editor.documents().any(|doc| doc.is_modified()
                && doc.path().is_some_and(|path| path.starts_with(&workspace))),
            "save or undo the unsaved buffers before previewing a pull request review"
        );

        let mut files = Files::new(pr, &workspace);
        let mut suggestions = Vec::new();
        let mut versions = Vec::new();
        let mut names = git(
            &[
                "diff",
                "--name-only",
                "-z",
                "--no-renames",
                &pr.head_oid,
                "--",
            ],
            &workspace,
        )?;
        names.extend(git(
            &["ls-files", "--others", "--exclude-standard", "-z"],
            &workspace,
        )?);
        let mut seen = HashSet::new();
        for name in names
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
        {
            let path = std::str::from_utf8(name).context("a changed filename is not UTF-8")?;
            if !seen.insert(path) {
                continue;
            }
            ensure!(seen.len() <= 512, "more than 512 saved changed files; narrow the checkout before previewing the review");
            match files.get(path) {
                Ok(file) => {
                    let mut candidates = file.suggestions(pr, path)?;
                    if candidates.is_empty() {
                        candidates.push(Suggestion {
                            id: fingerprint(&(&pr.head_oid, path, "metadata-only"))?,
                            path: path.to_owned(), start_line: 1, line: 1,
                            replacement: String::new(), original: String::new(), accepted: false,
                            error: Some("Git reports a file or metadata change with no text lines; GitHub cannot express it as an inline suggestion".to_owned()),
                        });
                    }
                    suggestions.extend(candidates);
                    versions.push((path.to_owned(), fingerprint(&file.saved)?));
                }
                Err(err) => {
                    let error = format!("{err:#}");
                    let metadata =
                        std::fs::symlink_metadata(workspace.join(path))
                            .ok()
                            .map(|metadata| {
                                (
                                    metadata.len(),
                                    metadata.modified().ok().map(|time| format!("{time:?}")),
                                )
                            });
                    let id = fingerprint(&(&pr.head_oid, path, &error, metadata))?;
                    versions.push((path.to_owned(), id.clone()));
                    suggestions.push(Suggestion {
                        id,
                        path: path.to_owned(),
                        start_line: 1,
                        line: 1,
                        replacement: String::new(),
                        original: String::new(),
                        accepted: false,
                        error: Some(error),
                    });
                }
            }
        }
        self.accepted
            .retain(|id| suggestions.iter().any(|suggestion| suggestion.id == *id));
        for suggestion in &mut suggestions {
            suggestion.accepted = self.accepted.contains(&suggestion.id);
        }

        let mut comments = Vec::new();
        let mut errors = Vec::new();
        for comment in &editor.review_pr_comments {
            let path = relative(&comment.path, &workspace)?;
            let anchored = self.comment_anchor(comment, pr, || {
                files.get(&path).and_then(|file| anchor(comment, file, pr))
            });
            let (side, start_line, line, code) = match anchored {
                Ok(anchor) => anchor,
                Err(err) => {
                    errors.push(format!("Comment on {path}: {err:#}"));
                    // Keep the draft removable even when its original anchor is stale.
                    (
                        side(comment.side),
                        comment.start_line,
                        comment.end_line,
                        comment.selected_text.clone(),
                    )
                }
            };
            comments.push(Comment {
                id: comment_id(comment)?,
                path,
                side,
                start_line,
                line,
                body: comment.comment.clone(),
                code,
                reply_to: comment.reply_to,
            });
        }

        let selection = selection_comments(editor, "", false)
            .ok()
            .and_then(|comments| comments.into_iter().next())
            .and_then(|comment| {
                let path = relative(&comment.path, &workspace).ok()?;
                let file = files.get(&path).ok()?;
                let (side, start_line, line, code) = anchor(&comment, file, pr).ok()?;
                let id = fingerprint(&(&pr.head_oid, &path, side, start_line, line, &code)).ok()?;
                Some(Selection {
                    id,
                    path,
                    side,
                    start_line,
                    line,
                    code,
                })
            });
        ensure_head(pr, &workspace)?;
        let pr = Pr {
            url: pr.url.clone(),
            repo: pr.repo.clone(),
            number: pr.number,
            head_sha: pr.head_oid.clone(),
            title: pr.title.clone(),
        };
        // Cursor movement is deliberately absent: selection.id binds adds separately.
        versions.sort_by(|a, b| a.0.cmp(&b.0));
        let error = (!errors.is_empty()).then(|| errors.join("; "));
        self.comment_anchors
            .retain(|id, _| comments.iter().any(|comment| &comment.id == id));
        let revision = fingerprint(&(&pr, &comments, &suggestions, &versions, &error))?;
        let snapshot = Snapshot {
            revision,
            pr,
            selection,
            comments,
            suggestions,
            error,
        };
        ensure!(serde_json::to_vec(&snapshot)?.len() <= MAX_SNAPSHOT,
            "pull request review preview exceeds 48 KiB; reduce the pending comments or saved edits");
        if self
            .snapshots
            .back()
            .is_none_or(|previous| previous.revision != snapshot.revision)
        {
            self.snapshots.push_back(snapshot.clone());
            if self.snapshots.len() > HISTORY {
                self.snapshots.pop_front();
            }
        }
        Ok(Some(snapshot))
    }

    pub(crate) fn comment(&mut self, editor: &mut Editor, text: &str) -> Result<()> {
        let request: CommentRequest =
            serde_json::from_str(text).context("invalid pull request comment action")?;
        let snapshot = self.require_revision(editor, &request.revision)?;
        let before = editor.review_pr_comments.clone();
        if let Some(id) = request.id {
            ensure!(
                request.selection_id.is_none(),
                "editing a comment does not accept a selection ID"
            );
            let idx = editor
                .review_pr_comments
                .iter()
                .position(|comment| comment_id(comment).ok().as_ref() == Some(&id))
                .context("the pending comment no longer exists; refresh the review")?;
            if let Some(body) = request.body {
                ensure!(!body.trim().is_empty(), "the comment is empty");
                editor.review_pr_comments[idx].comment = body;
            } else {
                editor.review_pr_comments.remove(idx);
            }
        } else {
            let body = request.body.context("adding a comment requires a body")?;
            ensure!(!body.trim().is_empty(), "the comment is empty");
            let selection = snapshot
                .selection
                .context("select unchanged pull request code before adding a comment")?;
            ensure!(
                request.selection_id.as_ref() == Some(&selection.id),
                "the code selection changed; refresh before adding the comment"
            );
            // Ordinary inline comments; desktop replies remain an explicit separate kind.
            let comment = selection_comments(editor, &body, false)?.remove(0);
            // Freeze the already validated selection before another filesystem read.
            self.comment_anchor(&comment, editor.review_pr.as_ref().unwrap(), || {
                Ok((
                    selection.side,
                    selection.start_line,
                    selection.line,
                    selection.code,
                ))
            })?;
            editor.review_pr_comments.push(comment);
        }
        if let Err(err) = self.snapshot(editor) {
            editor.review_pr_comments = before;
            return Err(err);
        }
        editor.share_review_comments();
        editor.set_status("Pull request comment draft updated");
        Ok(())
    }

    pub(crate) fn suggestion(&mut self, editor: &Editor, text: &str) -> Result<()> {
        let request: SuggestionRequest =
            serde_json::from_str(text).context("invalid suggestion action")?;
        let snapshot = self.require_revision(editor, &request.revision)?;
        let suggestion = snapshot
            .suggestions
            .iter()
            .find(|suggestion| suggestion.id == request.id)
            .context("the saved edit changed; refresh before accepting it")?;
        ensure!(
            !request.accepted || suggestion.error.is_none(),
            "{}",
            suggestion
                .error
                .as_deref()
                .unwrap_or("the saved edit cannot become a GitHub suggestion")
        );
        if request.accepted {
            self.accepted.insert(request.id);
        } else {
            self.accepted.remove(&request.id);
        }
        Ok(())
    }

    pub(crate) fn complete(&mut self, editor: &mut Editor, text: &str) -> Result<()> {
        let request: CompleteRequest =
            serde_json::from_str(text).context("invalid review completion action")?;
        // Refresh first: changed files invalidate their previous accepted suggestions.
        let current = self
            .snapshot(editor)?
            .context("not reviewing a pull request")?;
        let submitted = self
            .snapshots
            .iter()
            .find(|snapshot| snapshot.revision == request.revision)
            .context("the submitted review preview expired; pending entries were kept")?
            .clone();
        ensure!(
            current.pr == submitted.pr,
            "the pull request changed; pending entries were kept"
        );
        editor.review_pr_comments.retain(|comment| {
            let Ok(id) = comment_id(comment) else {
                return true;
            };
            !submitted
                .comments
                .iter()
                .any(|old| old.id == id && old.body == comment.comment)
        });
        for suggestion in submitted
            .suggestions
            .iter()
            .filter(|suggestion| suggestion.accepted)
        {
            self.accepted.remove(&suggestion.id);
        }
        // Forget completed previews so a repeated ack cannot clear later re-created entries.
        self.snapshots
            .retain(|snapshot| snapshot.revision != request.revision);
        editor.share_review_comments();
        editor.set_status("Published pull request review acknowledged; local edits kept");
        Ok(())
    }

    fn require_revision(&mut self, editor: &Editor, revision: &str) -> Result<Snapshot> {
        let snapshot = self
            .snapshot(editor)?
            .context("not reviewing a pull request")?;
        ensure!(
            snapshot.revision == revision,
            "the pull request review changed; refresh the preview and try again"
        );
        Ok(snapshot)
    }

    fn comment_anchor(
        &mut self,
        comment: &ReviewComment,
        pr: &PullRequest,
        resolve: impl FnOnce() -> Result<Anchor>,
    ) -> Result<Anchor> {
        let id = comment_id(comment)?;
        let capture = fingerprint(&(
            &pr.head_oid,
            &pr.merge_base,
            &comment.path,
            side(comment.side),
            &comment.baseline,
            comment.start_line,
            comment.end_line,
            &comment.selected_text,
            &comment.context_before,
            &comment.context_after,
        ))?;
        if let Some((previous, anchor)) = self.comment_anchors.get(&id) {
            if previous == &capture {
                return Ok(anchor.clone());
            }
        }
        let anchor = resolve()?;
        self.comment_anchors.insert(id, (capture, anchor.clone()));
        Ok(anchor)
    }
}

fn side(side: ReviewSide) -> &'static str {
    match side {
        ReviewSide::Old => "LEFT",
        ReviewSide::Current => "RIGHT",
    }
}

fn comment_id(comment: &ReviewComment) -> Result<String> {
    fingerprint(&(
        &comment.path,
        side(comment.side),
        comment.start_line,
        comment.end_line,
        format!("{:?}", comment.created),
    ))
}

fn fingerprint(value: &impl Serialize) -> Result<String> {
    Ok(Sha256::digest(serde_json::to_vec(value)?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn relative(path: &Path, workspace: &Path) -> Result<String> {
    Ok(path
        .strip_prefix(workspace)
        .context("review file is outside this workspace")?
        .to_str()
        .context("review filename is not UTF-8")?
        .replace('\\', "/"))
}

fn git(args: &[&str], workspace: &Path) -> Result<Vec<u8>> {
    let output = std::process::Command::new("git")
        .arg("--literal-pathspecs")
        .args(args)
        .current_dir(workspace)
        .output()?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.first().unwrap_or(&""),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

fn ensure_head(pr: &PullRequest, workspace: &Path) -> Result<()> {
    let head = git(&["rev-parse", "HEAD"], workspace)?;
    ensure!(String::from_utf8_lossy(&head).trim() == pr.head_oid,
        "the checked-out commit changed since this PR was attached; attach the current PR head again");
    Ok(())
}

struct Files<'a> {
    pr: &'a PullRequest,
    workspace: &'a Path,
    entries: HashMap<String, File>,
}

impl<'a> Files<'a> {
    fn new(pr: &'a PullRequest, workspace: &'a Path) -> Self {
        Self {
            pr,
            workspace,
            entries: HashMap::new(),
        }
    }

    fn get(&mut self, path: &str) -> Result<&File> {
        if !self.entries.contains_key(path) {
            let full = self.workspace.join(path);
            let saved = match std::fs::symlink_metadata(&full) {
                Ok(metadata) => {
                    ensure!(
                        metadata.is_file() && metadata.len() <= MAX_FILE,
                        "{path}: suggestions require a regular file no larger than 1 MiB"
                    );
                    ensure!(
                        full.canonicalize()?
                            .starts_with(self.workspace.canonicalize()?),
                        "{path}: saved file resolves outside this workspace"
                    );
                    let mut saved = String::new();
                    std::fs::File::open(&full)?
                        .take(MAX_FILE + 1)
                        .read_to_string(&mut saved)
                        .with_context(|| format!("{path}: saved file is not UTF-8"))?;
                    ensure!(
                        saved.len() as u64 <= MAX_FILE,
                        "{path}: saved file exceeds 1 MiB"
                    );
                    saved
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(err) => return Err(err.into()),
            };
            ensure!(
                !saved.contains('\0'),
                "{path}: binary files cannot be review suggestions"
            );
            let head = blob(&self.pr.head_oid, path, self.workspace)?;
            let head_exists = head.is_some();
            let head = head.unwrap_or_default();
            ensure!(
                head.len() as u64 <= MAX_FILE && !head.contains('\0'),
                "{path}: PR head is too large or binary"
            );
            let base = blob(&self.pr.merge_base, path, self.workspace)?.unwrap_or_default();
            let patch = git(
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--no-renames",
                    "-U3",
                    &self.pr.merge_base,
                    &self.pr.head_oid,
                    "--",
                    path,
                ],
                self.workspace,
            )?;
            let anchors = patch_ranges(&String::from_utf8_lossy(&patch));
            self.entries.insert(
                path.to_owned(),
                File {
                    head,
                    head_exists,
                    saved,
                    base,
                    anchors,
                },
            );
        }
        Ok(&self.entries[path])
    }
}

fn blob(revision: &str, path: &str, workspace: &Path) -> Result<Option<String>> {
    let entry = git(&["ls-tree", "-z", revision, "--", path], workspace)?;
    if entry.is_empty() {
        return Ok(None);
    }
    ensure!(
        entry.starts_with(b"100644 ") || entry.starts_with(b"100755 "),
        "{path}: only regular Git files support inline review drafts"
    );
    let object = format!("{revision}:{path}");
    let size = git(&["cat-file", "-s", &object], workspace)?;
    let size: u64 = std::str::from_utf8(&size)?.trim().parse()?;
    ensure!(size <= MAX_FILE, "{path}: Git file exceeds 1 MiB");
    let bytes = git(&["show", &object], workspace)?;
    ensure!(
        bytes.len() as u64 <= MAX_FILE && !bytes.contains(&0),
        "{path}: Git file is too large or binary"
    );
    String::from_utf8(bytes)
        .map(Some)
        .with_context(|| format!("{path}: Git file is not UTF-8"))
}

struct File {
    head: String,
    head_exists: bool,
    saved: String,
    base: String,
    anchors: Vec<(std::ops::Range<usize>, std::ops::Range<usize>)>,
}

impl File {
    fn suggestions(&self, pr: &PullRequest, path: &str) -> Result<Vec<Suggestion>> {
        let old: Vec<_> = self.head.lines().collect();
        let new: Vec<_> = self.saved.lines().collect();
        if !self.head_exists || (old.is_empty() && self.head != self.saved) {
            let replacement = replacement_lines(&new);
            let id = fingerprint(&(&pr.head_oid, path, &replacement))?;
            return Ok(vec![Suggestion { id, path: path.to_owned(), start_line: 1, line: 1,
                replacement, original: String::new(), accepted: false,
                error: Some("GitHub needs an existing PR diff line to anchor a suggestion; new or empty files cannot be inline suggestions".to_owned()) }]);
        }
        helix_vcs::line_hunks(self.head.as_bytes(), self.saved.as_bytes()).into_iter().map(|hunk| {
            ensure!(!old.is_empty(), "{path}: GitHub cannot anchor a suggestion to an empty PR file");
            let start = hunk.before.start as usize;
            let end = hunk.before.end as usize;
            let mut added: Vec<_> = new[hunk.after.start as usize..hunk.after.end as usize].to_vec();
            let (start_line, line) = if start == end {
                if start == 0 { added.push(old[0]); (1, 1) }
                else { added.insert(0, old[start - 1]); (start, start) }
            } else { (start + 1, end) };
                let replacement = replacement_lines(&added);
                let original = replacement_lines(&old[start_line - 1..line]);
            let id = fingerprint(&(&pr.head_oid, path, start_line, line, &replacement, &original))?;
            let error = if replacement == original {
                Some("GitHub inline suggestions cannot express a change only to line endings or the final newline".to_owned())
            } else { (!self.can_anchor("RIGHT", start_line, line)).then(||
                "These saved edits fall outside the PR diff; GitHub cannot attach an inline suggestion here".to_owned()) };
            Ok(Suggestion { id, path: path.to_owned(), start_line, line, replacement, original, accepted: false, error })
        }).collect()
    }

    fn can_anchor(&self, side: &str, start: usize, end: usize) -> bool {
        self.anchors.iter().any(|(left, right)| {
            let range = if side == "LEFT" { left } else { right };
            range.contains(&start) && range.contains(&end)
        })
    }
}

/// A trailing newline encodes each logical line, including a blank final line.
/// Zero replacement lines (a deletion) is the empty string; one blank line is `\n`.
fn replacement_lines(lines: &[&str]) -> String {
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    text
}

fn patch_ranges(patch: &str) -> Vec<(std::ops::Range<usize>, std::ops::Range<usize>)> {
    fn span(value: &str) -> Option<std::ops::Range<usize>> {
        let (start, count) = value.split_once(',').unwrap_or((value, "1"));
        let start: usize = start.parse().ok()?;
        Some(start..start + count.parse::<usize>().ok()?)
    }
    patch
        .lines()
        .filter_map(|line| {
            let header = line.strip_prefix("@@ -")?;
            let (old, rest) = header.split_once(" +")?;
            let new = rest.split_whitespace().next()?;
            Some((span(old)?, span(new)?))
        })
        .collect()
}

/// Find the captured code in the immutable side of the PR. Context also prevents a
/// comment on locally edited code from silently being moved to a different occurrence.
fn anchor(comment: &ReviewComment, file: &File, pr: &PullRequest) -> Result<Anchor> {
    let text = match comment.side {
        ReviewSide::Old => {
            ensure!(
                comment.baseline.revision() == Some(pr.merge_base.as_str()),
                "the old-side baseline changed; remove and recreate this comment"
            );
            &file.base
        }
        ReviewSide::Current => &file.head,
    };
    let fragment = format!(
        "{}{}{}",
        comment.context_before, comment.selected_text, comment.context_after
    );
    ensure!(
        !fragment.is_empty(),
        "empty files do not have an inline review anchor"
    );
    let locate = |text: &str| -> Option<(usize, usize)> {
        let mut matches = text.match_indices(&fragment);
        let offset = matches.next()?.0;
        if matches.next().is_some() {
            return None;
        }
        let from = offset + comment.context_before.len();
        let to = from + comment.selected_text.len();
        let start = text[..from].bytes().filter(|byte| *byte == b'\n').count() + 1;
        let last = if to > from { to - 1 } else { from };
        let line = text.as_bytes()[..last]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count()
            + 1;
        Some((start, line))
    };
    let (start, line) = if comment.side == ReviewSide::Old {
        locate(text).context(
            "selected old code changed or is ambiguous; remove and recreate this comment",
        )?
    } else {
        // Always map the actual saved selection first. Searching HEAD directly could
        // attach a newly inserted duplicate to an unrelated original occurrence.
        let (start, line) = locate(&file.saved)
            .context("selected code changed or is ambiguous; remove and recreate this comment")?;
        let mut shift = 0isize;
        for hunk in helix_vcs::line_hunks(file.head.as_bytes(), file.saved.as_bytes()) {
            let after = hunk.after.start as usize..hunk.after.end as usize;
            if after.end <= start - 1 {
                shift += hunk.before.len() as isize - hunk.after.len() as isize;
            } else {
                ensure!(after.start >= line,
                    "selected lines contain local edits; accept those edits as a suggestion instead");
            }
        }
        (
            (start as isize + shift) as usize,
            (line as isize + shift) as usize,
        )
    };
    let code = text
        .lines()
        .skip(start - 1)
        .take(line - start + 1)
        .collect::<Vec<_>>()
        .join("\n");
    ensure!(
        start <= line && line <= text.lines().count(),
        "selection is past the end of the PR file"
    );
    ensure!(
        file.can_anchor(side(comment.side), start, line),
        "selected lines are outside the PR diff; GitHub cannot attach an inline comment here"
    );
    Ok((side(comment.side), start, line, code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_core::{Range, Rope};
    use helix_view::review::ReviewBaseline;

    fn pr() -> PullRequest {
        PullRequest {
            number: 7,
            title: "Example".to_owned(),
            url: "https://github.com/o/r/pull/7".to_owned(),
            repo: "o/r".to_owned(),
            head_oid: "head".to_owned(),
            base_ref: "main".to_owned(),
            merge_base: "base".to_owned(),
            previous: "feature".to_owned(),
            batch_dir: None,
        }
    }

    fn file(base: &str, head: &str, saved: &str) -> File {
        File {
            base: base.to_owned(),
            head: head.to_owned(),
            head_exists: true,
            saved: saved.to_owned(),
            anchors: vec![(1..1000, 1..1000)],
        }
    }

    fn comment(text: &str, side: ReviewSide, start: usize, end: usize) -> ReviewComment {
        let rope = Rope::from(text);
        ReviewComment::new(
            "file.txt".into(),
            side,
            &ReviewBaseline::GitCommit {
                commit_oid: "base".to_owned(),
                blob_oid: Some("blob".to_owned()),
            },
            rope.slice(..),
            Range::new(rope.line_to_char(start - 1), rope.line_to_char(end)),
            start - 1,
            "literal @text\n%{buffer_name}".to_owned(),
        )
    }

    #[test]
    fn mobile_suggestions_anchor_insertions_deletions_and_invalidate_changed_content() {
        let pr = pr();
        let top = file("", "one\ntwo\n", "new\none\ntwo\n")
            .suggestions(&pr, "file.txt")
            .unwrap();
        assert_eq!((top[0].start_line, top[0].line), (1, 1));
        assert_eq!(top[0].replacement, "new\none\n");
        assert_eq!(top[0].original, "one\n");
        assert!(!top[0].accepted && top[0].error.is_none());
        let below = file("", "one\ntwo\n", "one\nnew\ntwo\n")
            .suggestions(&pr, "file.txt")
            .unwrap();
        assert_eq!((below[0].start_line, below[0].line), (1, 1));
        assert_eq!(below[0].replacement, "one\nnew\n");
        assert_ne!(top[0].id, below[0].id);
        let changed = file("", "one\ntwo\n", "other\none\ntwo\n")
            .suggestions(&pr, "file.txt")
            .unwrap();
        assert_ne!(top[0].id, changed[0].id);
        let deleted = file("", "one\ntwo\n", "")
            .suggestions(&pr, "file.txt")
            .unwrap();
        assert_eq!((deleted[0].start_line, deleted[0].line), (1, 2));
        assert_eq!(deleted[0].replacement, "");
    }

    #[test]
    fn mobile_suggestions_explain_unrepresentable_edits_without_accepting_them() {
        let pr = pr();
        let new = file("", "", "new\n").suggestions(&pr, "new.txt").unwrap();
        assert_eq!(new[0].replacement, "new\n");
        assert!(new[0]
            .error
            .as_deref()
            .unwrap()
            .contains("existing PR diff line"));
        let newline = file("", "one", "one\n")
            .suggestions(&pr, "file.txt")
            .unwrap();
        assert!(newline[0]
            .error
            .as_deref()
            .unwrap()
            .contains("final newline"));
        let mut outside = file("", "one\n", "ONE\n");
        outside.anchors.clear();
        let outside = outside.suggestions(&pr, "file.txt").unwrap();
        assert!(outside[0]
            .error
            .as_deref()
            .unwrap()
            .contains("outside the PR diff"));
        let mut empty_new = file("", "", "");
        empty_new.head_exists = false;
        let empty_new = empty_new.suggestions(&pr, "empty.txt").unwrap();
        assert_eq!(empty_new.len(), 1);
        assert!(empty_new[0].error.is_some());
    }

    #[test]
    fn mobile_suggestions_preserve_single_and_trailing_blank_replacement_lines() {
        let pr = pr();
        let deleted = file("", "old\n", "").suggestions(&pr, "file.txt").unwrap();
        let blank = file("", "old\n", "\n")
            .suggestions(&pr, "file.txt")
            .unwrap();
        assert_eq!(deleted[0].replacement, "");
        assert_eq!(blank[0].replacement, "\n");
        assert_ne!(deleted[0].id, blank[0].id);
        let trailing = file("", "old\n", "new\n\n")
            .suggestions(&pr, "file.txt")
            .unwrap();
        let single = file("", "old\n", "new\n")
            .suggestions(&pr, "file.txt")
            .unwrap();
        assert_eq!(trailing[0].replacement, "new\n\n");
        assert_eq!(single[0].replacement, "new\n");
        assert_ne!(trailing[0].id, single[0].id);
    }

    #[test]
    fn mobile_comments_map_multiline_saved_selections_to_the_immutable_head() {
        let head = "a\nB\nC\nd\n";
        let saved = "inserted\na\nB\nC\nd\n";
        let file = file("a\nb\nc\nd\n", head, saved);
        let captured = comment(saved, ReviewSide::Current, 3, 4);
        assert_eq!(
            anchor(&captured, &file, &pr()).unwrap(),
            ("RIGHT", 2, 3, "B\nC".to_owned())
        );
        let mut edited = file;
        edited.saved = saved.replace("B\n", "local edit\n");
        let changed = comment(&edited.saved, ReviewSide::Current, 3, 4);
        assert!(anchor(&changed, &edited, &pr())
            .unwrap_err()
            .to_string()
            .contains("local edits"));
    }

    #[test]
    fn mobile_comments_keep_old_multiline_anchors_on_a_deleted_pr_file() {
        let base = "one\ntwo\nthree\n";
        let file = file(base, "", "");
        let comment = comment(base, ReviewSide::Old, 2, 3);
        assert_eq!(
            anchor(&comment, &file, &pr()).unwrap(),
            ("LEFT", 2, 3, "two\nthree".to_owned())
        );
        let mut changed_base = pr();
        changed_base.merge_base = "another".to_owned();
        assert!(anchor(&comment, &file, &changed_base).is_err());
    }

    #[test]
    fn mobile_comments_reject_a_locally_inserted_duplicate_instead_of_moving_it() {
        let block = "a\nb\nc\ntarget\nd\ne\nf\n";
        let saved = format!("{block}separator\n{block}");
        let file = file("", block, &saved);
        let duplicate = comment(&saved, ReviewSide::Current, 12, 12);
        assert!(anchor(&duplicate, &file, &pr()).is_err());
    }

    #[test]
    fn mobile_existing_comments_keep_their_head_anchor_after_agent_and_body_edits() {
        let pr = pr();
        let original = file("old\nkeep\n", "head\nkeep\n", "head\nkeep\n");
        let mut captured = comment(&original.saved, ReviewSide::Current, 1, 1);
        let mut draft = Draft::default();
        let frozen = draft
            .comment_anchor(&captured, &pr, || anchor(&captured, &original, &pr))
            .unwrap();
        assert_eq!(frozen, ("RIGHT", 1, 1, "head".to_owned()));
        let modified = file("old\nkeep\n", "head\nkeep\n", "agent edit\nkeep\n");
        assert!(anchor(&captured, &modified, &pr).is_err());
        captured.comment = "edited body after the agent fix".to_owned();
        assert_eq!(
            draft
                .comment_anchor(&captured, &pr, || panic!(
                    "validated anchors must remain immutable"
                ))
                .unwrap(),
            frozen
        );
        let fresh = comment(&modified.saved, ReviewSide::Current, 1, 1);
        assert!(draft
            .comment_anchor(&fresh, &pr, || anchor(&fresh, &modified, &pr))
            .is_err());
        let mut new_pr = pr.clone();
        new_pr.head_oid = "new head".to_owned();
        assert!(draft
            .comment_anchor(&captured, &new_pr, || Err(anyhow::anyhow!("head changed")))
            .is_err());
    }

    #[test]
    fn mobile_comment_ids_survive_body_edits_and_keep_numeric_thread_ids() {
        let mut comment = comment("one\ntwo\n", ReviewSide::Current, 1, 1);
        let id = comment_id(&comment).unwrap();
        comment.comment = "changed".to_owned();
        assert_eq!(comment_id(&comment).unwrap(), id);
        let wire = Comment {
            id,
            path: "file.txt".to_owned(),
            side: "RIGHT",
            start_line: 1,
            line: 1,
            body: comment.comment,
            code: "one".to_owned(),
            reply_to: Some(42),
        };
        assert_eq!(serde_json::to_value(wire).unwrap()["replyTo"], 42);
    }

    #[test]
    fn mobile_patch_ranges_preserve_sides_and_single_line_headers() {
        let ranges = patch_ranges("@@ -0,0 +1,3 @@\n+new\n@@ -9 +12,2 @@ text\n");
        assert_eq!(ranges, [(0..0, 1..4), (9..10, 12..14)]);
    }
}
