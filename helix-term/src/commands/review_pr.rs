//! Reviewing GitHub pull requests through the `gh` CLI, run off the UI thread as jobs.

use std::process::Stdio;

use serde::Deserialize;

use helix_view::editor::ReviewMode;
use helix_view::graphics::{CursorKind, Margin, Rect};
use helix_view::review::{PullRequest, ReviewSide, ReviewThread, ThreadComment, ThreadSide};
use tui::buffer::Buffer as Surface;
use tui::widgets::{Block, Widget};

use crate::job::Job;
use crate::{alt, ctrl, key, shift};

use super::review::{changed_files_since, ensure_reviews_saved, pin_review_baseline, plural};
use super::*;

/// The `gh` executable: `$HELIX_GH` when set, else `gh` on `PATH`.
pub(super) fn gh_program() -> String {
    std::env::var("HELIX_GH").unwrap_or_else(|_| "gh".to_owned())
}

/// Runs `program` with `args` in `cwd` and returns its stdout, or an error carrying its stderr.
pub(super) async fn run(program: &str, args: &[&str], cwd: &Path) -> anyhow::Result<String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => {
                anyhow!("{program} not found: install the GitHub CLI or set HELIX_GH")
            }
            _ => anyhow!("failed to run {program}: {err}"),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let what: Vec<_> = args.iter().take(2).copied().collect();
        bail!("{program} {} failed: {}", what.join(" "), stderr.trim());
    }
    Ok(String::from_utf8(output.stdout)?)
}

pub(super) const LOADING_MESSAGE: &str =
    "pull request is loading; navigation will be ready when checkout finishes";

const PR_FIELDS: &str =
    "number,title,url,headRefOid,baseRefName,baseRefOid,isCrossRepository,headRepositoryOwner";

/// Attach the already checked-out PR without changing branches, the index or saved edits.
pub(super) fn review_pr_attach(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    ensure!(
        cx.editor.review_mode != Some(ReviewMode::Work),
        "This is Work together; open a separate Review a PR session"
    );
    ensure!(!cx.editor.review_pr_loading, LOADING_MESSAGE);
    let workspace = helix_loader::find_workspace().0;
    ensure!(
        !cx.editor
            .documents()
            .any(|doc| doc.is_modified()
                && doc.path().is_some_and(|path| path.starts_with(&workspace))),
        "save or undo unsaved buffers before attaching a pull request"
    );
    let arg = args[0].to_owned();
    ensure!(cx.editor.review_mode != Some(ReviewMode::Review) || cx.editor.review_pr.as_ref().is_none_or(|pr| pr.url == arg), "This Review a PR session keeps its attached snapshot; open another session for a different PR");
    if cx.editor.review_pr.as_ref().is_some_and(|pr| pr.url == arg) {
        let pr = cx.editor.review_pr.as_ref().unwrap();
        let head = git(&["rev-parse", "HEAD"], &workspace)?;
        ensure!(
            String::from_utf8_lossy(&head).trim() == pr.head_oid,
            "the checked-out commit changed since the pull request was attached"
        );
        return Ok(());
    }
    ensure!(
        cx.editor.review_pr_comments.is_empty(),
        "pending pull request comments must be submitted or removed before attaching another PR"
    );
    cx.editor.review_pr_loading = true;
    cx.editor
        .set_status(format!("Attaching pull request {arg}…"));
    cx.jobs.callback(async move {
        let result = fetch_attachment(&arg, &workspace).await;
        let callback = move |editor: &mut Editor| {
            editor.review_pr_loading = false;
            let result =
                result.and_then(|pr| {
                    let head = git(&["rev-parse", "HEAD"], &workspace)?;
                    ensure!(
                        String::from_utf8_lossy(&head).trim() == pr.head_oid,
                        "the checkout changed while the PR was loading; attach it again"
                    );
                    ensure!(!editor.documents().any(|doc| doc.is_modified()
                    && doc.path().is_some_and(|path| path.starts_with(&workspace))),
                    "buffers changed while the PR was loading; save or undo before attaching it");
                    ensure!(
                        editor.review_pr_comments.is_empty(),
                        "pending comments changed while the PR was loading; they were kept"
                    );
                    start(editor, pr)
                });
            match result {
                Ok(pr) => Some(load_threads(pr)),
                Err(err) => {
                    editor.set_error(format!("{err:#}"));
                    None
                }
            }
        };
        Ok(Callback::Followup(Box::new(callback)))
    });
    Ok(())
}

async fn fetch_attachment(arg: &str, cwd: &Path) -> anyhow::Result<PullRequest> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct View {
        number: u64,
        title: String,
        url: String,
        head_ref_oid: String,
        base_ref_name: String,
        base_ref_oid: String,
    }
    let view: View = serde_json::from_str(
        &run(
            &gh_program(),
            &["pr", "view", arg, "--json", PR_FIELDS],
            cwd,
        )
        .await?,
    )
    .context("unexpected gh pr view output")?;
    let head = run("git", &["rev-parse", "HEAD"], cwd).await?;
    ensure!(
        head.trim() == view.head_ref_oid,
        "this checkout is not PR #{}'s current head; check out the PR before attaching it",
        view.number
    );
    let merge_base = run(
        "git",
        &["merge-base", &view.base_ref_oid, &view.head_ref_oid],
        cwd,
    )
    .await
    .context("PR base commit is unavailable locally; fetch the base branch before attaching")?;
    let previous = run("git", &["symbolic-ref", "--short", "-q", "HEAD"], cwd)
        .await
        .unwrap_or_else(|_| head.clone());
    let url = helix_stdx::Url::parse(&view.url).context("invalid PR URL from gh")?;
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    ensure!(
        parts.len() == 4 && parts[2] == "pull" && parts[3] == view.number.to_string(),
        "unexpected PR URL from gh"
    );
    Ok(PullRequest {
        number: view.number,
        title: view.title,
        repo: format!("{}/{}", parts[0], parts[1]),
        url: view.url,
        head_oid: view.head_ref_oid,
        base_ref: view.base_ref_name,
        merge_base: merge_base.trim().to_owned(),
        previous: previous.trim().to_owned(),
        batch_dir: None,
    })
}

/// Looks up pull request `arg` (a number or URL), checks it out and finds its merge base.
async fn fetch_and_checkout(
    arg: &str,
    batch_dir: Option<PathBuf>,
    cwd: &Path,
) -> anyhow::Result<PullRequest> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct View {
        number: u64,
        title: String,
        url: String,
        head_ref_oid: String,
        base_ref_name: String,
        base_ref_oid: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Repo {
        name_with_owner: String,
    }

    let gh = gh_program();
    let view: View =
        serde_json::from_str(&run(&gh, &["pr", "view", arg, "--json", PR_FIELDS], cwd).await?)
            .context("unexpected gh pr view output")?;
    let repo: Repo =
        serde_json::from_str(&run(&gh, &["repo", "view", "--json", "nameWithOwner"], cwd).await?)
            .context("unexpected gh repo view output")?;
    let previous = match run("git", &["symbolic-ref", "--short", "-q", "HEAD"], cwd).await {
        Ok(branch) => branch,
        Err(_) => run("git", &["rev-parse", "HEAD"], cwd).await?,
    };
    run(&gh, &["pr", "checkout", &view.number.to_string()], cwd).await?;
    let merge_base = ["merge-base", view.base_ref_oid.as_str(), "HEAD"];
    let merge_base = match run("git", &merge_base, cwd).await {
        Ok(oid) => oid,
        Err(_) => {
            // The base commit may not be fetched yet.
            run(
                "git",
                &["fetch", "--quiet", "origin", &view.base_ref_oid],
                cwd,
            )
            .await?;
            run("git", &merge_base, cwd).await?
        }
    };
    Ok(PullRequest {
        number: view.number,
        title: view.title,
        url: view.url,
        repo: repo.name_with_owner,
        head_oid: view.head_ref_oid,
        base_ref: view.base_ref_name,
        merge_base: merge_base.trim().to_owned(),
        previous: previous.trim().to_owned(),
        batch_dir,
    })
}

pub(super) fn review_pr(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    ensure!(cx.editor.review_mode.is_none(), "Open a separate Review a PR session to choose a pull request; this session keeps its checkout unchanged");
    ensure!(!cx.editor.review_pr_loading, LOADING_MESSAGE);
    if &args[0] == "close" {
        let pr = cx
            .editor
            .review_pr
            .take()
            .context("not reviewing a pull request")?;
        let restored = end_review(cx.editor, &pr);
        cx.editor
            .set_status(format!("Stopped reviewing PR #{}; {restored}", pr.number));
        return Ok(());
    }
    let workspace = helix_loader::find_workspace().0;
    let uncommitted: Vec<_> = changed_files_since(cx.editor, "HEAD")?
        .into_iter()
        .filter(|change| !matches!(change, helix_vcs::FileChange::Untracked { .. }))
        .collect();
    if let Some(change) = uncommitted.first() {
        bail!(
            "the worktree has uncommitted changes ({}); commit or stash them before :review-pr",
            change
                .path()
                .strip_prefix(&workspace)
                .unwrap_or(change.path())
                .display()
        );
    }
    ensure_reviews_saved(cx.editor, ":review-pr")?;
    let arg = args[0].to_owned();
    let batch_dir = args.get(1).map(helix_stdx::path::canonicalize);
    cx.editor
        .set_status(format!("Checking out pull request {arg}…"));
    cx.editor.review_pr_loading = true;
    cx.jobs.callback(async move {
        let result = fetch_and_checkout(&arg, batch_dir, &workspace).await;
        let callback = move |editor: &mut Editor| {
            editor.review_pr_loading = false;
            match result.and_then(|pr| start(editor, pr)) {
                Ok(pr) => Some(load_threads(pr)),
                Err(err) => {
                    editor.set_error(format!("{err:#}"));
                    None
                }
            }
        };
        Ok(Callback::Followup(Box::new(callback)))
    });
    Ok(())
}

/// Re-pins the pull request's baseline when its merge base with `HEAD` moved, as after the
/// branch was rebased onto a newer base.
pub(super) fn follow_merge_base(editor: &mut Editor) -> anyhow::Result<()> {
    if editor.review_mode == Some(ReviewMode::Review) {
        return Ok(());
    }
    let Some(pr) = &editor.review_pr else {
        return Ok(());
    };
    let workspace = helix_loader::find_workspace().0;
    let trust_full = editor
        .workspace_trust
        .query(&workspace, helix_loader::workspace_trust::TrustQuery::Git)
        .is_trusted();
    let base = [format!("origin/{}", pr.base_ref), pr.base_ref.clone()]
        .into_iter()
        .find_map(|base| {
            editor
                .diff_providers
                .merge_base_with_head(&workspace, &base, trust_full)
                .ok()
        })
        .context("the pull request's base branch is not in this repository")?;
    if base == pr.merge_base {
        return Ok(());
    }
    pin_review_baseline(editor, &base)?;
    if let Some(pr) = editor.review_pr.as_mut() {
        pr.merge_base = base.clone();
    }
    editor.review_user_pin = None;
    editor.set_status(format!("Baseline moved to {} after a rebase", &base[..8]));
    Ok(())
}

/// The merge base of `HEAD` with the branch it is based on: the pull request's base while
/// reviewing one, else `origin/HEAD`, else the first of the usual base branch names that exists.
pub(super) fn base_merge_base(editor: &Editor) -> Option<String> {
    let workspace = helix_loader::find_workspace().0;
    let trust_full = editor
        .workspace_trust
        .query(&workspace, helix_loader::workspace_trust::TrustQuery::Git)
        .is_trusted();
    let pr = editor
        .review_pr
        .iter()
        .flat_map(|pr| [format!("origin/{}", pr.base_ref), pr.base_ref.clone()]);
    let usual = [
        "origin/HEAD",
        "origin/develop",
        "origin/main",
        "origin/master",
        "develop",
        "main",
        "master",
    ]
    .map(str::to_owned);
    pr.chain(usual).find_map(|base| {
        editor
            .diff_providers
            .merge_base_with_head(&workspace, &base, trust_full)
            .ok()
    })
}

pub(super) fn review_submit(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    ensure!(
        cx.editor.review_mode != Some(ReviewMode::Review),
        "Use Publish review to submit suggestion drafts; this session keeps the checkout unchanged"
    );
    let pr = cx
        .editor
        .review_pr
        .clone()
        .context("not reviewing a pull request, use :review-pr first")?;
    let text = args.first().unwrap_or_default().trim();
    if text.is_empty() && !cx.editor.socket_command {
        let modal = SubmitModal::new(cx.editor, pr)?;
        cx.jobs.callback(async move {
            let callback = move |_: &mut Editor, compositor: &mut Compositor| {
                compositor.push(Box::new(modal));
            };
            Ok(Callback::EditorCompositor(Box::new(callback)))
        });
        return Ok(());
    }
    let (event, summary) = match text.split_once(char::is_whitespace).unwrap_or((text, "")) {
        ("approve", rest) => ("APPROVE", rest.trim()),
        ("request-changes", rest) => ("REQUEST_CHANGES", rest.trim()),
        ("comment", rest) => ("COMMENT", rest.trim()),
        _ => ("COMMENT", text),
    };
    submit(cx.editor, &pr, event, summary)
}

/// What `:review-submit` hands to gtc: the pending comments and the suggested changes made of the
/// reviewed buffers' differences from the pull request's head, as review batch entries.
struct Submission {
    comments: Vec<serde_json::Value>,
    /// How many of `comments` reply to a thread.
    replies: usize,
    suggestions: Vec<serde_json::Value>,
    /// The files the suggestions were made of.
    files: Vec<PathBuf>,
}

impl Submission {
    fn new(editor: &Editor) -> anyhow::Result<Submission> {
        ensure_reviews_saved(editor, ":review-submit")?;
        let workspace = helix_loader::find_workspace().0;
        let pending = &editor.review_pr_comments;
        let mut merged = vec![false; pending.len()];
        let mut suggestions = Vec::new();
        let mut files = Vec::new();
        for doc in editor.documents().filter(|doc| doc.review.is_some()) {
            let Some(path) = doc.path().filter(|path| path.starts_with(&workspace)) else {
                continue;
            };
            let relative = relative_path(path, &workspace);
            let diff = git(&["diff", "-U0", "HEAD", "--", &relative], &workspace)?;
            let diff = String::from_utf8_lossy(&diff);
            if diff.is_empty() {
                continue;
            }
            let head = git(&["show", &format!("HEAD:{relative}")], &workspace)?;
            let head_lines: Vec<_> = String::from_utf8_lossy(&head)
                .lines()
                .map(str::to_owned)
                .collect();
            let old_line = |line: usize| head_lines.get(line - 1).cloned().unwrap_or_default();
            for hunk in DiffHunk::parse(&diff) {
                let (start, end, new) = match hunk.old_count {
                    // A pure insertion replaces the line above it, or below at the top.
                    0 if hunk.old_start == 0 => (1, 1, [hunk.added, vec![old_line(1)]].concat()),
                    0 => (
                        hunk.old_start,
                        hunk.old_start,
                        [vec![old_line(hunk.old_start)], hunk.added].concat(),
                    ),
                    count => (hunk.old_start, hunk.old_start + count - 1, hunk.added),
                };
                let comment = pending.iter().enumerate().position(|(idx, comment)| {
                    !merged[idx]
                        && comment.reply_to.is_none()
                        && comment.side == ReviewSide::Current
                        && comment.path == *path
                        && (comment.start_line, comment.end_line) == (start, end)
                });
                if let Some(idx) = comment {
                    merged[idx] = true;
                }
                suggestions.push(serde_json::json!({
                    "path": relative,
                    "start_line": start,
                    "line": end,
                    "replacement": new.join("\n"),
                    "comment": comment.map(|idx| &pending[idx].comment),
                }));
            }
            files.push(path.to_path_buf());
        }

        let unmerged: Vec<_> = pending
            .iter()
            .zip(merged)
            .filter(|(_, merged)| !merged)
            .map(|(comment, _)| comment)
            .collect();
        let comments = unmerged
            .iter()
            .map(|comment| {
                serde_json::json!({
                    "path": relative_path(&comment.path, &workspace),
                    "side": match comment.side {
                        ReviewSide::Current => "RIGHT",
                        ReviewSide::Old => "LEFT",
                    },
                    "start_line": comment.start_line,
                    "line": comment.end_line,
                    "body": comment.comment,
                    "reply_to": comment.reply_to,
                })
            })
            .collect();
        Ok(Submission {
            comments,
            replies: unmerged.iter().filter(|c| c.reply_to.is_some()).count(),
            suggestions,
            files,
        })
    }
}

/// `path` relative to `workspace` with `/` separators, as GitHub names files.
fn relative_path(path: &Path, workspace: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// A hunk of `git diff -U0`: its 1-based old line range and the lines replacing it.
struct DiffHunk {
    old_start: usize,
    old_count: usize,
    added: Vec<String>,
}

impl DiffHunk {
    fn parse(diff: &str) -> Vec<DiffHunk> {
        let mut hunks: Vec<DiffHunk> = Vec::new();
        for line in diff.lines() {
            if let Some(header) = line.strip_prefix("@@ -") {
                let old = header.split(' ').next().unwrap_or_default();
                let (start, count) = old.split_once(',').unwrap_or((old, "1"));
                hunks.push(DiffHunk {
                    old_start: start.parse().unwrap_or(0),
                    old_count: count.parse().unwrap_or(1),
                    added: Vec::new(),
                });
            } else if let (Some(hunk), Some(added)) = (hunks.last_mut(), line.strip_prefix('+')) {
                hunk.added.push(added.to_owned());
            }
        }
        hunks
    }
}

/// Runs git with `args` in `cwd` on this thread and returns its stdout.
fn git(args: &[&str], cwd: &Path) -> anyhow::Result<Vec<u8>> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.first().unwrap_or(&""),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

/// Checks out what was checked out before `pr` was, unless the worktree has uncommitted changes,
/// and returns the status saying which.
pub(crate) fn restore_branch(pr: &PullRequest, cwd: &Path) -> String {
    let restore = || -> anyhow::Result<String> {
        let status = git(&["status", "--porcelain"], cwd)?;
        if String::from_utf8_lossy(&status)
            .lines()
            .any(|line| !line.starts_with("??"))
        {
            let branch = git(&["symbolic-ref", "--short", "-q", "HEAD"], cwd)
                .map(|branch| String::from_utf8_lossy(&branch).trim().to_owned())
                .unwrap_or_else(|_| pr.head_oid.clone());
            return Ok(format!(
                "kept {branch} checked out: the worktree has uncommitted changes"
            ));
        }
        git(&["checkout", "-q", &pr.previous], cwd)?;
        Ok(format!("back on {}", pr.previous))
    };
    restore().unwrap_or_else(|err| format!("could not check out {}: {err:#}", pr.previous))
}

/// Leaves the review of `pr`, taken off the editor, checks out what was checked out before it and
/// reloads the unmodified buffers, returning the status of the checkout.
fn end_review(editor: &mut Editor, pr: &PullRequest) -> String {
    editor.review_user_pin = None;
    editor.review_baseline = None;
    editor.review_hook_baseline = None;
    editor.review_nav = None;
    editor.review_pr_threads.clear();
    editor.share_review_threads();
    let workspace = helix_loader::find_workspace().0;
    let restored = restore_branch(pr, &workspace);
    let trust_full = editor
        .workspace_trust
        .query(&workspace, helix_loader::workspace_trust::TrustQuery::Git)
        .is_trusted();
    for doc in editor.documents.values_mut() {
        doc.close_review(&editor.diff_providers, trust_full);
    }
    let doc_ids: Vec<_> = editor
        .documents()
        .filter(|doc| !doc.is_modified() && doc.path().is_some_and(|path| path.exists()))
        .map(Document::id)
        .collect();
    for doc_id in doc_ids {
        if let Err(err) = super::typed::reload_document(editor, doc_id) {
            log::info!("not reloading a buffer after the pull request review: {err:#}");
        }
    }
    restored
}

/// Hands the review of `pr` with `event` and `summary` to gtc as a review batch in the
/// pull request's batch directory; then clears the pending comments, reverts the files the
/// suggestions were made of and ends the pull request review.
fn submit(editor: &mut Editor, pr: &PullRequest, event: &str, summary: &str) -> anyhow::Result<()> {
    let dir = pr
        .batch_dir
        .as_deref()
        .context("no reviewer attached: open the pull request from gtc prs")?;
    let Submission {
        comments,
        replies,
        suggestions,
        files,
    } = Submission::new(editor)?;
    ensure!(
        !comments.is_empty()
            || !suggestions.is_empty()
            || !summary.is_empty()
            || event != "COMMENT",
        "nothing to submit: no pending pull request comments, no edits and no summary"
    );
    let (comment_count, suggestion_count) = (comments.len() - replies, suggestions.len());
    let batch = serde_json::json!({
        "version": 1,
        "kind": "pull-request-review",
        "repo": pr.repo,
        "number": pr.number,
        "head": pr.head_oid,
        "event": event,
        "body": summary,
        "comments": comments,
        "suggestions": suggestions,
    });
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    let path = dir.join(format!("review-{}-{millis}.json", pr.number));
    let tmp = dir.join(format!("review-{}-{millis}.json.tmp", pr.number));
    // The rename makes the batch appear whole to gtc.
    std::fs::write(&tmp, serde_json::to_vec_pretty(&batch)?)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
        .with_context(|| format!("writing the review batch to {} failed", dir.display()))?;

    editor.review_pr_comments.clear();
    if !files.is_empty() {
        let workspace = helix_loader::find_workspace().0;
        let mut args = vec!["checkout", "--"];
        args.extend(files.iter().filter_map(|file| file.to_str()));
        git(&args, &workspace).with_context(|| {
            format!(
                "handed the review of #{} to gtc, but reverting the suggested edits failed",
                pr.number
            )
        })?;
    }
    editor.review_pr = None;
    let restored = end_review(editor, pr);
    editor.set_status(format!(
        "Handed the review of #{} to gtc: {}, {}, {}; {restored}",
        pr.number,
        plural(comment_count, "comment"),
        plural(replies, "reply"),
        plural(suggestion_count, "suggestion")
    ));
    Ok(())
}

const VERBS: [(&str, &str); 3] = [
    ("comment", "COMMENT"),
    ("approve", "APPROVE"),
    ("request changes", "REQUEST_CHANGES"),
];

/// The modal `:review-submit` opens: the review's verb, its summary and what it will post.
pub(super) struct SubmitModal {
    pr: PullRequest,
    footer: String,
    verb: usize,
    summary: String,
    /// The byte offset of the cursor in `summary`.
    cursor: usize,
    /// Where the summary was last rendered.
    summary_area: Rect,
}

impl SubmitModal {
    pub(super) fn new(editor: &Editor, pr: PullRequest) -> anyhow::Result<SubmitModal> {
        let submission = Submission::new(editor)?;
        let footer = format!(
            "{}, {}, {} · Enter submits · Esc cancels",
            plural(submission.comments.len() - submission.replies, "comment"),
            plural(submission.replies, "reply"),
            plural(submission.suggestions.len(), "suggestion")
        );
        Ok(SubmitModal {
            pr,
            footer,
            verb: 0,
            summary: String::new(),
            cursor: 0,
            summary_area: Rect::default(),
        })
    }

    fn line_start(&self) -> usize {
        self.summary[..self.cursor]
            .rfind('\n')
            .map_or(0, |idx| idx + 1)
    }

    fn line_end(&self) -> usize {
        self.summary[self.cursor..]
            .find('\n')
            .map_or(self.summary.len(), |idx| self.cursor + idx)
    }

    fn insert(&mut self, text: &str) {
        self.summary.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    fn prev_char(&self) -> Option<usize> {
        let ch = self.summary[..self.cursor].chars().next_back()?;
        Some(self.cursor - ch.len_utf8())
    }

    fn next_char(&self) -> Option<usize> {
        let ch = self.summary[self.cursor..].chars().next()?;
        Some(self.cursor + ch.len_utf8())
    }

    /// Moves the cursor to the line before (or after) its own, keeping its column if it can.
    fn move_line(&mut self, up: bool) {
        let start = self.line_start();
        let column = self.summary[start..self.cursor].chars().count();
        let line = match up {
            true if start > 0 => self.summary[..start - 1]
                .rfind('\n')
                .map_or(0, |idx| idx + 1),
            false if self.line_end() < self.summary.len() => self.line_end() + 1,
            _ => return,
        };
        let text = &self.summary[line..];
        let text = &text[..text.find('\n').unwrap_or(text.len())];
        self.cursor = line
            + text
                .char_indices()
                .nth(column)
                .map_or(text.len(), |(idx, _)| idx);
    }
}

impl Component for SubmitModal {
    fn handle_event(
        &mut self,
        event: &compositor::Event,
        cx: &mut compositor::Context,
    ) -> compositor::EventResult {
        use compositor::EventResult;
        use helix_view::keyboard::KeyModifiers;

        let key = match event {
            compositor::Event::Key(key) => *key,
            compositor::Event::Paste(text) => {
                self.insert(text);
                return EventResult::Consumed(None);
            }
            compositor::Event::Resize(..) | compositor::Event::Mouse(_) => {
                return EventResult::Consumed(None)
            }
            _ => return EventResult::Ignored(None),
        };
        let close = || {
            EventResult::Consumed(Some(Box::new(|compositor: &mut Compositor, _: &mut _| {
                compositor.pop();
            })))
        };
        match key {
            key!(Esc) | ctrl!('c') => return close(),
            key!(Enter) => {
                let summary = self.summary.trim().to_owned();
                let event = VERBS[self.verb].1;
                if let Err(err) = submit(cx.editor, &self.pr, event, &summary) {
                    cx.editor.set_error(format!("{err:#}"));
                }
                return close();
            }
            alt!(Enter) | shift!(Enter) => self.insert("\n"),
            key!(Tab) => self.verb = (self.verb + 1) % VERBS.len(),
            shift!(Tab) => self.verb = (self.verb + VERBS.len() - 1) % VERBS.len(),
            key!(Right) if self.summary.is_empty() => self.verb = (self.verb + 1).min(2),
            key!(Left) if self.summary.is_empty() => self.verb = self.verb.saturating_sub(1),
            key!(Right) => self.cursor = self.next_char().unwrap_or(self.cursor),
            key!(Left) => self.cursor = self.prev_char().unwrap_or(self.cursor),
            key!(Up) => self.move_line(true),
            key!(Down) => self.move_line(false),
            key!(Home) => self.cursor = self.line_start(),
            key!(End) => self.cursor = self.line_end(),
            key!(Backspace) => {
                if let Some(prev) = self.prev_char() {
                    self.summary.replace_range(prev..self.cursor, "");
                    self.cursor = prev;
                }
            }
            key!(Delete) => {
                if let Some(next) = self.next_char() {
                    self.summary.replace_range(self.cursor..next, "");
                }
            }
            KeyEvent {
                code: KeyCode::Char(ch),
                modifiers,
            } if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                self.insert(ch.encode_utf8(&mut [0; 4]))
            }
            _ => (),
        }
        EventResult::Consumed(None)
    }

    fn render(&mut self, viewport: Rect, surface: &mut Surface, cx: &mut compositor::Context) {
        let popup = cx.editor.theme.get("ui.popup");
        let text = cx.editor.theme.get("ui.text");
        let selected = cx.editor.theme.get("ui.menu.selected");
        let rows = self.summary.split('\n').count().max(3) as u16;
        let width = viewport.width.saturating_sub(4).min(80);
        let height = (rows + 4).min(viewport.height);
        let area = viewport.intersection(Rect::new(
            viewport.x + (viewport.width - width) / 2,
            viewport.y + (viewport.height - height) / 2,
            width,
            height,
        ));
        surface.clear_with(area, popup);
        let title = format!("Review #{} {}", self.pr.number, self.pr.title);
        let block = Block::bordered().title(title.as_str()).border_style(popup);
        let inner = block.inner(area).inner(Margin::horizontal(1));
        block.render(area, surface);
        if inner.height < 3 {
            return;
        }

        let mut verbs = Vec::new();
        for (idx, (verb, _)) in VERBS.iter().enumerate() {
            if idx > 0 {
                verbs.push(Span::styled("  ", text));
            }
            verbs.push(match idx == self.verb {
                true => Span::styled(format!("[{verb}]"), selected),
                false => Span::styled(*verb, text),
            });
        }
        surface.set_spans(inner.x, inner.y, &Spans::from(verbs), inner.width);
        self.summary_area = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 2);
        for (row, line) in self.summary.split('\n').enumerate() {
            let y = self.summary_area.y + row as u16;
            if y < self.summary_area.bottom() {
                surface.set_stringn(inner.x, y, line, inner.width as usize, text);
            }
        }
        surface.set_stringn(
            inner.x,
            inner.bottom() - 1,
            &self.footer,
            inner.width as usize,
            text,
        );
    }

    fn cursor(&self, _area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        use helix_core::unicode::width::UnicodeWidthStr;

        let row = self.summary[..self.cursor].matches('\n').count();
        let column = self.summary[self.line_start()..self.cursor].width();
        (
            Some(Position::new(
                self.summary_area.y as usize + row,
                self.summary_area.x as usize + column,
            )),
            editor.config().cursor_shape.from_mode(Mode::Insert),
        )
    }
}

/// A job loading the review comment threads of `pr` and showing them in the reviews.
pub(super) fn load_threads(pr: PullRequest) -> Job {
    Job::with_callback(async move {
        let workspace = helix_loader::find_workspace().0;
        let threads = fetch_threads(&pr, &workspace).await;
        let callback = move |editor: &mut Editor| match threads {
            // A pull request closed or replaced meanwhile keeps its own threads.
            Ok(threads) if editor.review_pr.as_ref() == Some(&pr) => {
                editor.review_pr_threads = threads;
                editor.share_review_threads();
            }
            Ok(_) => (),
            Err(err) => editor.set_error(format!("loading the PR comments failed: {err:#}")),
        };
        Ok(Callback::Editor(Box::new(callback)))
    })
}

/// The review comment threads of `pr` with paths under `workspace`, oldest first.
async fn fetch_threads(pr: &PullRequest, workspace: &Path) -> anyhow::Result<Vec<ReviewThread>> {
    #[derive(Deserialize)]
    struct User {
        login: String,
    }
    #[derive(Deserialize)]
    struct Comment {
        id: u64,
        in_reply_to_id: Option<u64>,
        path: String,
        line: Option<usize>,
        original_line: Option<usize>,
        side: Option<String>,
        body: String,
        user: User,
    }
    let endpoint = format!("repos/{}/pulls/{}/comments", pr.repo, pr.number);
    let output = run(&gh_program(), &["api", &endpoint, "--paginate"], workspace).await?;
    // --paginate prints one JSON array per page.
    let mut comments = Vec::new();
    for page in serde_json::Deserializer::from_str(&output).into_iter::<Vec<Comment>>() {
        comments.extend(page.context("unexpected gh api output")?);
    }
    let mut threads: Vec<ReviewThread> = Vec::new();
    let mut roots = std::collections::HashMap::new();
    for comment in comments {
        let entry = ThreadComment {
            id: comment.id,
            login: comment.user.login,
            body: comment.body,
        };
        match comment.in_reply_to_id.and_then(|root| roots.get(&root)) {
            Some(&idx) => {
                let thread: &mut ReviewThread = &mut threads[idx];
                thread.comments.push(entry);
            }
            None => {
                roots.insert(comment.id, threads.len());
                threads.push(ReviewThread {
                    path: workspace.join(&comment.path),
                    side: match comment.side.as_deref() {
                        Some("LEFT") => ThreadSide::Left,
                        _ => ThreadSide::Right,
                    },
                    line: comment.line,
                    original_line: comment.original_line,
                    comments: vec![entry],
                });
            }
        }
    }
    Ok(threads)
}

/// Reviews every buffer against the pull request's merge base and enters PR mode.
fn start(editor: &mut Editor, pr: PullRequest) -> anyhow::Result<PullRequest> {
    editor.nav_mode = helix_view::editor::NavMode::Review;
    editor.review_pr = None;
    editor.review_user_pin = None;
    if editor.review_mode == Some(ReviewMode::Review) {
        editor.review_pr = Some(pr.clone());
        let workspace = helix_loader::find_workspace().0;
        let docs: Vec<_> = editor
            .documents()
            .filter(|doc| doc.path().is_some_and(|path| path.starts_with(&workspace)))
            .map(Document::id)
            .collect();
        for id in docs {
            if let Err(error) = editor.initialize_review_draft(id) {
                editor.review_pr = None;
                editor.review_baseline = None;
                editor.review_nav = None;
                return Err(error);
            }
            editor.launch_language_servers(id);
        }
        editor.review_baseline = Some(pr.merge_base.clone());
        editor.review_nav = None;
    } else {
        pin_review_baseline(editor, &pr.merge_base)?;
    }
    let files = changed_files_since(editor, &pr.merge_base)?.len();
    editor.set_status(format!(
        "Reviewing PR #{} {}: {} against {} at {}",
        pr.number,
        pr.title,
        plural(files, "file"),
        pr.base_ref,
        &pr.merge_base[..8.min(pr.merge_base.len())]
    ));
    editor.review_pr_threads.clear();
    editor.review_pr_comments.clear();
    editor.share_review_threads();
    editor.review_pr = Some(pr.clone());
    Ok(pr)
}
