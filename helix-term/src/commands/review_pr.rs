//! Reviewing GitHub pull requests through the `gh` CLI, run off the UI thread as jobs.

use std::process::Stdio;

use serde::Deserialize;

use helix_view::review::{
    PullRequest, ReviewComment, ReviewSide, ReviewThread, ThreadComment, ThreadSide,
};

use crate::job::Job;

use super::review::{changed_files_since, ensure_reviews_saved, pin_review_baseline, plural};
use super::*;

/// The `gh` executable: `$HELIX_GH` when set, else `gh` on `PATH`.
pub(super) fn gh_program() -> String {
    std::env::var("HELIX_GH").unwrap_or_else(|_| "gh".to_owned())
}

/// Runs `program` with `args` in `cwd`, feeding it `input`, and returns its stdout, or an error
/// carrying its stderr.
pub(super) async fn run(
    program: &str,
    args: &[&str],
    cwd: &Path,
    input: Option<Vec<u8>>,
) -> anyhow::Result<String> {
    use tokio::io::AsyncWriteExt;

    let mut child = tokio::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => {
                anyhow!("{program} not found: install the GitHub CLI or set HELIX_GH")
            }
            _ => anyhow!("failed to run {program}: {err}"),
        })?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(&input).await?;
    }
    let output = child.wait_with_output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let what: Vec<_> = args.iter().take(2).copied().collect();
        bail!("{program} {} failed: {}", what.join(" "), stderr.trim());
    }
    Ok(String::from_utf8(output.stdout)?)
}

const PR_FIELDS: &str =
    "number,title,url,headRefOid,baseRefName,baseRefOid,isCrossRepository,headRepositoryOwner";

/// Looks up pull request `arg` (a number or URL), checks it out and finds its merge base.
async fn fetch_and_checkout(arg: &str, cwd: &Path) -> anyhow::Result<PullRequest> {
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
    let view: View = serde_json::from_str(
        &run(&gh, &["pr", "view", arg, "--json", PR_FIELDS], cwd, None).await?,
    )
    .context("unexpected gh pr view output")?;
    let repo: Repo = serde_json::from_str(
        &run(&gh, &["repo", "view", "--json", "nameWithOwner"], cwd, None).await?,
    )
    .context("unexpected gh repo view output")?;
    run(
        &gh,
        &["pr", "checkout", &view.number.to_string()],
        cwd,
        None,
    )
    .await?;
    let merge_base = ["merge-base", view.base_ref_oid.as_str(), "HEAD"];
    let merge_base = match run("git", &merge_base, cwd, None).await {
        Ok(oid) => oid,
        Err(_) => {
            // The base commit may not be fetched yet.
            run(
                "git",
                &["fetch", "--quiet", "origin", &view.base_ref_oid],
                cwd,
                None,
            )
            .await?;
            run("git", &merge_base, cwd, None).await?
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
    if &args[0] == "close" {
        let pr = cx
            .editor
            .review_pr
            .take()
            .context("not reviewing a pull request")?;
        cx.editor.review_user_pin = None;
        cx.editor.review_pr_threads.clear();
        cx.editor.share_review_threads();
        cx.editor
            .set_status(format!("Stopped reviewing PR #{}", pr.number));
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
    cx.editor
        .set_status(format!("Checking out pull request {arg}…"));
    cx.jobs.callback(async move {
        let result = fetch_and_checkout(&arg, &workspace).await;
        let callback = move |editor: &mut Editor| match result.and_then(|pr| start(editor, pr)) {
            Ok(pr) => Some(load_threads(pr)),
            Err(err) => {
                editor.set_error(format!("{err:#}"));
                None
            }
        };
        Ok(Callback::Followup(Box::new(callback)))
    });
    Ok(())
}

/// Re-pins the pull request's baseline when its merge base with `HEAD` moved, as after the
/// branch was rebased onto a newer base.
pub(super) fn follow_merge_base(editor: &mut Editor) -> anyhow::Result<()> {
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

/// Lines of context around a change that GitHub still accepts review comments on.
const DIFF_CONTEXT: usize = 3;

pub(super) fn review_submit(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let pr = cx
        .editor
        .review_pr
        .clone()
        .context("not reviewing a pull request, use :review-pr first")?;
    let text = args.first().unwrap_or_default().trim();
    let (event, summary) = match text.split_once(char::is_whitespace).unwrap_or((text, "")) {
        ("approve", rest) => ("APPROVE", rest.trim()),
        ("request-changes", rest) => ("REQUEST_CHANGES", rest.trim()),
        ("comment", rest) => ("COMMENT", rest.trim()),
        _ => ("COMMENT", text),
    };
    let pending = cx.editor.review_pr_comments.clone();
    ensure!(
        !pending.is_empty() || !summary.is_empty() || event != "COMMENT",
        "nothing to submit: no pending pull request comments and no summary"
    );
    let workspace = helix_loader::find_workspace().0;
    let mut comments = Vec::new();
    let mut replies = Vec::new();
    let mut file_level = 0;
    for comment in &pending {
        if let Some(root) = comment.reply_to {
            replies.push(serde_json::json!({ "body": comment.comment, "in_reply_to": root }));
            continue;
        }
        let path = comment
            .path
            .strip_prefix(&workspace)
            .unwrap_or(&comment.path)
            .to_string_lossy()
            .replace('\\', "/");
        let side = match comment.side {
            ReviewSide::Current => "RIGHT",
            ReviewSide::Old => "LEFT",
        };
        if in_diff(cx.editor, &pr, comment)? {
            let mut entry = serde_json::json!({
                "path": path,
                "line": comment.end_line,
                "side": side,
                "body": comment.comment,
            });
            if comment.start_line != comment.end_line {
                entry["start_line"] = comment.start_line.into();
                entry["start_side"] = side.into();
            }
            comments.push(entry);
        } else {
            file_level += 1;
            comments.push(serde_json::json!({
                "path": path,
                "subject_type": "file",
                "body": file_level_body(comment),
            }));
        }
    }
    let lines = comments.len() - file_level;
    let review = (!comments.is_empty() || !summary.is_empty() || event != "COMMENT").then(|| {
        serde_json::json!({
            "commit_id": pr.head_oid,
            "event": event,
            "body": summary,
            "comments": comments,
        })
    });
    cx.editor
        .set_status(format!("Submitting the review of #{}…", pr.number));
    cx.jobs.callback(async move {
        let result = post_review(&pr, review, replies.clone(), &workspace).await;
        let callback = move |editor: &mut Editor| match result {
            Ok(()) => {
                editor
                    .review_pr_comments
                    .retain(|comment| !pending.contains(comment));
                editor.set_status(format!(
                    "Submitted {event} on #{}: {}, {}, {} file-level",
                    pr.number,
                    plural(lines, "comment"),
                    plural(replies.len(), "reply"),
                    file_level
                ));
                Some(load_threads(pr))
            }
            Err(err) => {
                editor.set_error(format!("submitting the review failed: {err:#}"));
                None
            }
        };
        Ok(Callback::Followup(Box::new(callback)))
    });
    Ok(())
}

/// Posts `review` (if any) to the pull request, then each reply to its thread.
async fn post_review(
    pr: &PullRequest,
    review: Option<serde_json::Value>,
    replies: Vec<serde_json::Value>,
    cwd: &Path,
) -> anyhow::Result<()> {
    let gh = gh_program();
    if let Some(review) = review {
        let endpoint = format!("repos/{}/pulls/{}/reviews", pr.repo, pr.number);
        let args = ["api", "-X", "POST", &endpoint, "--input", "-"];
        run(&gh, &args, cwd, Some(serde_json::to_vec(&review)?)).await?;
    }
    let endpoint = format!("repos/{}/pulls/{}/comments", pr.repo, pr.number);
    for reply in replies {
        let args = ["api", "-X", "POST", &endpoint, "--input", "-"];
        run(&gh, &args, cwd, Some(serde_json::to_vec(&reply)?)).await?;
    }
    Ok(())
}

/// Whether GitHub accepts a line comment on `comment`'s lines: they must be changed lines of the
/// pull request (from its merge base), or within [`DIFF_CONTEXT`] lines of one, on their side.
fn in_diff(editor: &Editor, pr: &PullRequest, comment: &ReviewComment) -> anyhow::Result<bool> {
    let trust_full = editor
        .workspace_trust
        .query(
            &helix_loader::find_workspace().0,
            helix_loader::workspace_trust::TrustQuery::Git,
        )
        .is_trusted();
    let base = editor
        .diff_providers
        .get_commit_base(&comment.path, &pr.merge_base, trust_full)
        .map(|base| base.data)
        .unwrap_or_default();
    let current = match editor.document_by_path(&comment.path) {
        Some(doc) => doc.text().to_string().into_bytes(),
        None => std::fs::read(&comment.path).unwrap_or_default(),
    };
    let lines = comment.start_line - 1..comment.end_line;
    Ok(helix_vcs::line_hunks(&base, &current).iter().any(|hunk| {
        let changed = match comment.side {
            ReviewSide::Current => hunk.after.start as usize..hunk.after.end as usize,
            ReviewSide::Old => hunk.before.start as usize..hunk.before.end as usize,
        };
        let near = changed.start.saturating_sub(DIFF_CONTEXT)..changed.end + DIFF_CONTEXT;
        near.start <= lines.start && lines.end <= near.end
    }))
}

/// The body of a file-level comment for `comment`: its selected lines quoted with their line
/// numbers, then the comment.
fn file_level_body(comment: &ReviewComment) -> String {
    let side = match comment.side {
        ReviewSide::Current => "",
        ReviewSide::Old => "old ",
    };
    let mut body: String = comment
        .selected_text
        .lines()
        .enumerate()
        .map(|(idx, line)| format!("> {side}{}: {line}\n", comment.start_line + idx))
        .collect();
    body.push('\n');
    body.push_str(&comment.comment);
    body
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
    let output = run(
        &gh_program(),
        &["api", &endpoint, "--paginate"],
        workspace,
        None,
    )
    .await?;
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
    pin_review_baseline(editor, &pr.merge_base)?;
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
