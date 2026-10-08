//! Reviewing GitHub pull requests through the `gh` CLI, run off the UI thread as jobs.

use std::process::Stdio;

use serde::Deserialize;

use helix_view::review::{PullRequest, ReviewThread, ThreadComment, ThreadSide};

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
        cx.editor.review_stepped_from = None;
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
    editor.review_pr = None;
    editor.review_stepped_from = None;
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
