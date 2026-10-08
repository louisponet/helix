//! Reviewing GitHub pull requests through the `gh` CLI, run off the UI thread as jobs.

use std::process::Stdio;

use serde::Deserialize;

use helix_view::review::PullRequest;

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
        let callback = move |editor: &mut Editor| {
            if let Err(err) = result.and_then(|pr| start(editor, pr)) {
                editor.set_error(format!("{err:#}"));
            }
        };
        Ok(Callback::Editor(Box::new(callback)))
    });
    Ok(())
}

/// Reviews every buffer against the pull request's merge base and enters PR mode.
fn start(editor: &mut Editor, pr: PullRequest) -> anyhow::Result<()> {
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
    editor.review_pr = Some(pr);
    Ok(())
}
