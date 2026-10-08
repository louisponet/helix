//! Inline review of a buffer against an index or commit snapshot.

use std::collections::HashSet;
use std::ops;
use std::time::{SystemTime, UNIX_EPOCH};

use helix_core::command_line::Args;
use helix_core::doc_formatter::TextFormat;
use helix_core::graphemes::prev_grapheme_boundary;
use helix_core::line_ending::line_end_char_index;
use helix_core::text_annotations::TextAnnotations;
use helix_view::review::{OldFocus, Review, ReviewComment, ReviewRequest, ReviewSide};

use helix_loader::workspace_trust::TrustQuery;
use helix_vcs::FileChange;

use super::typed::{doc_trust_full, reload, reload_document};
use super::*;

const OLD_FOCUS_HELP: &str = "Old text (read-only): move, select, y yank, ]g/[g next/prev block, :review-comment <text>, Esc returns";

/// Typable commands allowed while old text is focused. Only `review-revert` modifies a document,
/// and it leaves the old text first.
const OLD_FOCUS_TYPABLE: &[&str] = &[
    "review-revert",
    "review-stage",
    "review-unstage",
    "review-toggle-stage",
    "review-comment",
    "review-comments",
    "review-uncomment",
    "review-export",
    "review-send",
    "review-commit",
    "review-close",
    "review-old",
];

/// Whether the focused view is inspecting old review text.
pub(crate) fn old_focused(editor: &Editor) -> bool {
    let (view, doc) = current_ref!(editor);
    doc.review
        .as_ref()
        .is_some_and(|review| review.focus(view.id).is_some())
}

pub(super) fn typable_allowed(editor: &Editor, name: &str) -> bool {
    !old_focused(editor) || OLD_FOCUS_TYPABLE.contains(&name)
}

/// Formats `count` followed by `noun`, pluralized with a trailing "s" unless `count` is 1.
fn plural(count: usize, noun: &str) -> String {
    let suffix = if count == 1 { "" } else { "s" };
    format!("{count} {noun}{suffix}")
}

fn record_reviewed_file(editor: &mut Editor) {
    let (_, doc) = current_ref!(editor);
    if let (Some(path), Some(review)) = (doc.path(), &doc.review) {
        let (path, baseline) = (path.to_path_buf(), review.baseline().clone());
        editor.review_batch.add_file(path, baseline);
    }
}

pub(super) fn review(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    // Without an argument, the editor-wide baseline wins so a reviewer stays on the agent's commit.
    let baseline = cx.editor.review_baseline.clone();
    let revision = match args.get(0) {
        Some("index") => None,
        Some(revision) => Some(revision),
        None => baseline.as_deref(),
    };
    start_review(cx, revision)
}

fn start_review(cx: &mut compositor::Context, revision: Option<&str>) -> anyhow::Result<()> {
    let trust_full = doc_trust_full(cx.editor);
    let doc = doc!(cx.editor);
    let dirty = doc.is_modified();
    ensure!(
        !dirty || !doc.is_modified_on_disk()?,
        "buffer has unsaved changes and was modified on disk; preserve your edits before :review"
    );
    let mut review = doc.load_review(&cx.editor.diff_providers, trust_full, revision)?;
    if !dirty {
        reload(cx, Args::default(), PromptEvent::Validate)?;
    }
    let doc = doc_mut!(cx.editor);
    review.update(doc.text());
    ensure!(!review.is_suspended(), Review::LIMIT_MESSAGE);
    let status = review.summary();
    doc.set_review(review);
    let scrolloff = cx.editor.config().scrolloff;
    let (view, doc) = current!(cx.editor);
    view.ensure_cursor_in_view(doc, scrolloff);
    record_reviewed_file(cx.editor);
    cx.editor.set_status(status);
    Ok(())
}

pub(super) fn review_commits(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let doc = doc!(cx.editor);
    let doc_id = doc.id();
    let path = doc.path().context("buffer has no path")?;
    let commits = cx
        .editor
        .diff_providers
        .get_commit_history(path, doc_trust_full(cx.editor))?;
    cx.jobs.callback(async move {
        let callback: job::Callback =
            Callback::EditorCompositor(Box::new(move |_editor, compositor| {
                let columns = [ui::PickerColumn::new(
                    "commit baseline (hash and subject)",
                    |commit: &helix_vcs::CommitInfo, _| {
                        format!("{} {}", commit.id, commit.summary).into()
                    },
                )];
                let picker =
                    ui::Picker::new(columns, 0, commits, (), move |cx, commit, _action| {
                        if doc!(cx.editor).id() != doc_id {
                            cx.editor
                                .set_error("review buffer changed; open :review-commits again");
                            return;
                        }
                        if let Err(error) = start_review(cx, Some(&commit.id)) {
                            cx.editor.set_error(format!("{error:#}"));
                        }
                    });
                compositor.push(Box::new(overlaid(picker)));
            }));
        Ok(callback)
    });
    Ok(())
}

pub(super) fn review_close(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let scrolloff = cx.editor.config().scrolloff;
    let trust_full = doc_trust_full(cx.editor);
    let baseline = cx.editor.review_baseline.clone();
    let (view, doc) = current!(cx.editor);
    if doc
        .close_review(&cx.editor.diff_providers, trust_full)
        .is_none()
    {
        bail!("not reviewing this buffer");
    }
    view.ensure_cursor_in_view(doc, scrolloff);
    // The editor-wide baseline would otherwise review the buffer again on its next refresh.
    let status = match &baseline {
        Some(commit) => format!("Review closed; :review {} resumes it", &commit[..8]),
        None => "Review closed".to_owned(),
    };
    doc.review_opt_out = baseline;
    cx.editor.set_status(status);
    Ok(())
}

pub(super) fn review_old(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    ensure_review(cx)?;

    let review = doc!(cx.editor)
        .review
        .as_ref()
        .context("not reviewing this buffer, use :review first")?;
    ensure!(!review.is_suspended(), Review::LIMIT_MESSAGE);
    ensure!(
        !review.blocks().is_empty(),
        "no baseline lines were removed"
    );

    cx.editor.enter_normal_mode();
    let scrolloff = cx.editor.config().scrolloff;
    let (view, doc) = current!(cx.editor);
    let cursor_line = doc
        .selection(view.id)
        .primary()
        .cursor_line(doc.text().slice(..));
    let review = doc.review.as_mut().unwrap();
    let blocks = review.blocks();
    let idx = blocks
        .iter()
        .position(|block| block.current.end.max(block.current.start + 1) > cursor_line)
        .unwrap_or(blocks.len() - 1);
    review.focus_block(view.id, idx);
    view.center_review_block_in_view(doc, scrolloff);
    cx.editor.set_status(OLD_FOCUS_HELP);
    Ok(())
}

pub(super) fn review_refresh(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    ensure_review(cx)?;
    let doc = doc!(cx.editor);
    let review = doc
        .review
        .as_ref()
        .context("not reviewing this buffer, use :review first")?;
    ensure!(
        !doc.is_modified(),
        "buffer has unsaved changes, write or undo them before :review-refresh"
    );
    let revision = review.baseline().revision().map(str::to_owned);
    start_review(cx, revision.as_deref())?;
    let baseline = doc!(cx.editor).review.as_ref().unwrap().baseline();
    cx.editor
        .set_status(format!("Review refreshed from disk and the {baseline}"));
    Ok(())
}

pub(super) fn review_revert(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    ensure_review(cx)?;

    let (view, doc) = current!(cx.editor);
    let line = doc
        .selection(view.id)
        .primary()
        .cursor_line(doc.text().slice(..));
    let review = doc
        .review
        .as_mut()
        .context("not reviewing this buffer, use :review first")?;
    ensure!(!review.is_suspended(), Review::LIMIT_MESSAGE);
    let (old, current) = match review.focused_block_index(view.id) {
        Some(idx) => {
            let block = &review.blocks()[idx];
            (block.old.clone(), block.current.clone())
        }
        // A removal has no current lines, so it is found by the line its old lines are shown at.
        None => review
            .hunk_at_line(line)
            .or_else(|| {
                let block = review
                    .blocks_at(line)
                    .iter()
                    .find(|block| block.current.is_empty())?;
                Some((block.old.clone(), block.current.clone()))
            })
            .context("no changed lines here")?,
    };
    review.clear_focus(view.id);
    let snapshot = review.snapshot().slice(..);
    let restored: Tendril = snapshot
        .slice(snapshot.line_to_char(old.start)..snapshot.line_to_char(old.end))
        .to_string()
        .into();
    let text = doc.text().slice(..);
    let start = text.line_to_char(current.start);
    let end = text.line_to_char(current.end);
    let transaction = Transaction::change(doc.text(), [(start, end, Some(restored))].into_iter())
        .with_selection(Selection::point(start));
    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);
    let scrolloff = cx.editor.config().scrolloff;
    let (view, doc) = current!(cx.editor);
    view.ensure_cursor_in_view(doc, scrolloff);
    let lines = if old.is_empty() {
        current.len()
    } else {
        old.len()
    };
    cx.editor.set_status(format!(
        "Reverted {} to the baseline",
        plural(lines, "line")
    ));
    Ok(())
}

pub(super) fn review_comment(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    ensure_review(cx)?;

    let (view, doc) = current_ref!(cx.editor);
    let review = doc
        .review
        .as_ref()
        .context("not reviewing this buffer, use :review first")?;
    ensure!(!review.is_suspended(), Review::LIMIT_MESSAGE);
    let path = doc.path().context("buffer has no path")?.to_path_buf();
    let text = args[0].to_string();
    let comment = match review
        .focus(view.id)
        .zip(review.focused_block_index(view.id))
    {
        Some((focus, idx)) => ReviewComment::new(
            path,
            ReviewSide::Old,
            review.baseline(),
            review.snapshot().slice(..),
            focus.range,
            review.blocks()[idx].current.start,
            text,
        ),
        None => {
            let doc_text = doc.text().slice(..);
            let range = doc.selection(view.id).primary();
            ReviewComment::new(
                path,
                ReviewSide::Current,
                review.baseline(),
                doc_text,
                range,
                doc_text.char_to_line(range.from()),
                text,
            )
        }
    };
    let pending = cx.editor.review_batch.add_comment(comment);
    cx.editor.set_status(format!(
        "{}, :review-export <path> writes them",
        plural(pending, "pending review comment")
    ));
    Ok(())
}

pub(super) fn review_comments(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    if let Err(err) = ensure_review(cx) {
        log::info!("not enabling review: {err:#}");
    }

    let comments = cx.editor.review_batch.comments().to_vec();
    ensure!(!comments.is_empty(), "no pending review comments");
    let workspace = helix_loader::find_workspace().0;
    cx.jobs.callback(async move {
        let callback: job::Callback =
            Callback::EditorCompositor(Box::new(move |_editor, compositor| {
                let columns = [
                    ui::PickerColumn::new(
                        "file",
                        |comment: &ReviewComment, workspace: &PathBuf| {
                            let path = &comment.path;
                            path.strip_prefix(workspace)
                                .unwrap_or(path)
                                .display()
                                .to_string()
                                .into()
                        },
                    ),
                    ui::PickerColumn::new("lines", |comment: &ReviewComment, _| {
                        let side = match comment.side {
                            ReviewSide::Old => "old",
                            ReviewSide::Current => "current",
                        };
                        match comment.start_line == comment.end_line {
                            true => format!("{side} {}", comment.start_line),
                            false => format!("{side} {}-{}", comment.start_line, comment.end_line),
                        }
                        .into()
                    }),
                    ui::PickerColumn::new("comment", |comment: &ReviewComment, _| {
                        comment.comment.as_str().into()
                    }),
                ];
                let picker =
                    ui::Picker::new(columns, 2, comments, workspace, |cx, comment, action| {
                        let doc_id = match cx.editor.open(&comment.path, action) {
                            Ok(id) => id,
                            Err(err) => {
                                cx.editor.set_error(format!(
                                    "failed to open {}: {err}",
                                    comment.path.display()
                                ));
                                return;
                            }
                        };
                        let view = view_mut!(cx.editor);
                        let doc = doc_mut!(cx.editor, &doc_id);
                        if let Some(review) = doc.review.as_mut() {
                            review.clear_focus(view.id);
                        }
                        let text = doc.text().slice(..);
                        let line = (comment.current_line - 1).min(text.len_lines() - 1);
                        doc.set_selection(view.id, Selection::point(text.line_to_char(line)));
                        if action.align_view(view, doc_id) {
                            align_view(doc, view, Align::Center);
                        }
                    })
                    .with_preview(|_editor, comment| {
                        let lines = match comment.side {
                            ReviewSide::Current => (comment.start_line - 1, comment.end_line - 1),
                            ReviewSide::Old => (comment.current_line - 1, comment.current_line - 1),
                        };
                        Some((comment.path.as_path().into(), Some(lines)))
                    });
                compositor.push(Box::new(overlaid(picker)));
            }));
        Ok(callback)
    });
    Ok(())
}

pub(super) fn review_uncomment(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    if let Err(err) = ensure_review(cx) {
        log::info!("not enabling review: {err:#}");
    }

    let count = match args.first() {
        Some(count) => count
            .parse::<usize>()
            .ok()
            .filter(|&count| count > 0)
            .context("count must be a positive number")?,
        None => 1,
    };
    let batch = &mut cx.editor.review_batch;
    ensure!(!batch.comments().is_empty(), "no pending review comments");
    let removed = batch.remove_latest_comments(count);
    let pending = batch.comments().len();
    cx.editor.set_status(format!(
        "Dropped {}, {pending} pending",
        plural(removed, "review comment")
    ));
    Ok(())
}

pub(super) fn review_export(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let path = helix_stdx::path::canonicalize(Path::new(&args[0]));
    ensure!(
        cx.editor.document_by_path(&path).is_none(),
        "{} is open in a buffer, export to a new file",
        path.display()
    );
    let reviewed: Vec<_> = cx
        .editor
        .documents()
        .filter_map(|doc| {
            let review = doc.review.as_ref()?;
            Some((doc.path()?.to_path_buf(), review.baseline().clone()))
        })
        .collect();
    for (file, baseline) in reviewed {
        cx.editor.review_batch.add_file(file, baseline);
    }
    ensure!(
        !cx.editor.review_batch.is_empty(),
        "nothing to export, use :review first"
    );
    let unsaved: Vec<_> = cx
        .editor
        .review_batch
        .files()
        .filter(|file| {
            cx.editor
                .document_by_path(file)
                .is_some_and(Document::is_modified)
        })
        .map(|file| file.display().to_string())
        .collect();
    ensure!(
        unsaved.is_empty(),
        "write reviewed buffers before exporting: {}",
        unsaved.join(", ")
    );

    for file in cx.editor.review_batch.files() {
        let doc = cx.editor.document_by_path(file).with_context(|| {
            format!("reopen {} and use :review before exporting", file.display())
        })?;
        ensure!(
            !doc.review.as_ref().is_some_and(Review::is_suspended),
            "{}: {}",
            file.display(),
            Review::LIMIT_MESSAGE
        );
        ensure!(
            !doc.is_modified_on_disk().with_context(|| format!(
                "cannot verify {} on disk; use :review-refresh before exporting",
                file.display()
            ))?,
            "{} changed on disk; use :review-refresh before exporting",
            file.display()
        );
    }

    let comments = cx.editor.review_batch.comments().len();
    let files = cx.editor.review_batch.files().count();
    cx.editor.review_batch.export(&path, |_| true)?;
    cx.editor.set_status(format!(
        "Exported {} on {} to {}",
        plural(comments, "comment"),
        plural(files, "file"),
        path.display()
    ));
    Ok(())
}

pub(super) fn review_baseline(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    if &args[0] == "clear" {
        cx.editor.review_baseline = None;
        let doc_ids: Vec<_> = cx.editor.documents().map(Document::id).collect();
        for doc_id in doc_ids {
            let doc = doc_mut!(cx.editor, &doc_id);
            let trust_full = cx
                .editor
                .workspace_trust
                .query(doc.workspace_root(), TrustQuery::Git)
                .is_trusted();
            doc.close_review(&cx.editor.diff_providers, trust_full);
            doc.review_opt_out = None;
        }
        let scrolloff = cx.editor.config().scrolloff;
        let (view, doc) = current!(cx.editor);
        view.ensure_cursor_in_view(doc, scrolloff);
        cx.editor.set_status("Review baseline cleared");
        return Ok(());
    }
    pin_review_baseline(cx, &args[0])
}

/// Enables review mode for a review command: pins `HEAD` as the editor-wide baseline when none is
/// set, then starts reviewing the current buffer against it if it is a workspace file without a
/// review that the reviewer did not close under that baseline.
fn ensure_review(cx: &mut compositor::Context) -> anyhow::Result<()> {
    let doc = doc!(cx.editor);
    // A buffer under review, whatever its baseline, or the diff buffer needs nothing more.
    if doc.review.is_some() || cx.editor.review_diff == Some(doc.id()) {
        return Ok(());
    }
    let commit = review_baseline_or_head(cx)?;
    let workspace = helix_loader::find_workspace().0;
    let doc = doc!(cx.editor);
    let start = doc.review.is_none()
        && doc.path().is_some_and(|path| path.starts_with(&workspace))
        && doc.review_opt_out.as_ref() != Some(&commit);
    if start {
        start_review(cx, Some(&commit))?;
    }
    Ok(())
}

/// The editor-wide review baseline, pinning `HEAD` like `:review-baseline HEAD` when none is set.
fn review_baseline_or_head(cx: &mut compositor::Context) -> anyhow::Result<String> {
    if cx.editor.review_baseline.is_none() {
        pin_review_baseline(cx, "HEAD")
            .context("no review baseline, and HEAD cannot be resolved")?;
    }
    Ok(cx.editor.review_baseline.clone().unwrap())
}

/// Pins `revision` as the editor-wide review baseline and reviews the workspace buffers against it.
fn pin_review_baseline(cx: &mut compositor::Context, revision: &str) -> anyhow::Result<()> {
    let workspace = helix_loader::find_workspace().0;
    let trust_full = cx
        .editor
        .workspace_trust
        .query(&workspace, TrustQuery::Git)
        .is_trusted();
    let commit = cx
        .editor
        .diff_providers
        .resolve_commit(&workspace, revision, trust_full)?;
    let refresh = cx.editor.review_baseline.as_ref() == Some(&commit);
    cx.editor.review_baseline = Some(commit.clone());

    let doc_ids: Vec<_> = cx
        .editor
        .documents()
        .filter(|doc| doc.path().is_some_and(|path| path.starts_with(&workspace)))
        .map(Document::id)
        .collect();
    let (mut reviewed, mut unchanged, mut unsaved, mut failed, mut closed) = (0, 0, 0, 0, 0);
    let mut conflicts = Vec::new();
    for doc_id in doc_ids {
        let doc = doc!(cx.editor, &doc_id);
        if doc.review_opt_out.as_ref() == Some(&commit) {
            closed += 1;
            continue;
        }
        if doc.is_modified() {
            if doc.is_modified_on_disk().unwrap_or(false) {
                conflicts.push(doc.display_name().into_owned());
            } else {
                unsaved += 1;
            }
            continue;
        }
        let current = doc
            .review
            .as_ref()
            .is_some_and(|review| review.baseline().revision() == Some(commit.as_str()));
        if refresh && current && doc.is_modified_on_disk().is_ok_and(|modified| !modified) {
            unchanged += 1;
            continue;
        }
        let revision = match &doc.review {
            Some(review) if refresh => review.baseline().revision().map(str::to_owned),
            _ => Some(commit.clone()),
        };
        let result = reload_document(cx.editor, doc_id).and_then(|()| {
            let doc = doc_mut!(cx.editor, &doc_id);
            let trust_full = cx
                .editor
                .workspace_trust
                .query(doc.workspace_root(), TrustQuery::Git)
                .is_trusted();
            let review =
                doc.load_review(&cx.editor.diff_providers, trust_full, revision.as_deref())?;
            doc.set_review(review);
            Ok(())
        });
        match result {
            Ok(()) => reviewed += 1,
            Err(err) => {
                log::info!("not reviewing document {doc_id}: {err:#}");
                failed += 1;
            }
        }
    }

    // Keep an open :review-diff buffer in step with the files it shows.
    refresh_review_diff(cx.editor);

    let scrolloff = cx.editor.config().scrolloff;
    let (view, doc) = current!(cx.editor);
    view.ensure_cursor_in_view(doc, scrolloff);
    let mut status =
        if reviewed == 0 && unsaved == 0 && failed == 0 && conflicts.is_empty() && unchanged > 0 {
            format!(
                "{} up to date against {}",
                plural(unchanged, "buffer"),
                &commit[..8]
            )
        } else {
            let mut status = format!(
                "Reviewing {} against {}",
                plural(reviewed, "buffer"),
                &commit[..8]
            );
            if unchanged > 0 {
                status.push_str(&format!("; {unchanged} unchanged"));
            }
            status
        };
    if closed > 0 {
        status.push_str(&format!("; {closed} closed"));
    }
    if unsaved > 0 {
        status.push_str(&format!("; {unsaved} unsaved skipped"));
    }
    if failed > 0 {
        status.push_str(&format!("; {failed} unreviewable skipped"));
    }
    if !conflicts.is_empty() {
        let more = if conflicts.len() > 3 { ", ..." } else { "" };
        conflicts.truncate(3);
        status.push_str(&format!(
            "; {}{more} also changed on disk: :reload discards your edits, :w! overwrites the agent's",
            conflicts.join(", ")
        ));
    }
    cx.editor.set_status(status);
    Ok(())
}

pub(super) fn review_handoff(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    if &args[0] == "clear" {
        cx.editor.review_handoff = None;
        cx.editor.set_status("Review handoff cleared");
        return Ok(());
    }
    let dir = PathBuf::from(&args[0]);
    ensure!(
        dir.is_absolute() && dir.is_dir(),
        "{} is not an absolute path to a directory",
        dir.display()
    );
    cx.editor.set_status(format!(
        "Writing a reviewed buffer exports a review batch to {}",
        dir.display()
    ));
    cx.editor.review_handoff = Some(dir);
    Ok(())
}

pub(super) fn review_send(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    if let Err(err) = ensure_review(cx) {
        log::info!("not enabling review: {err:#}");
    }
    let request = args.first().map(|text| ReviewRequest::Message {
        text: text.to_owned(),
    });
    let path = hand_off(cx.editor, request, ":review-send")?;
    cx.editor
        .set_status(format!("Exported review batch {}", path.display()));
    Ok(())
}

pub(super) fn review_commit(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    if let Err(err) = ensure_review(cx) {
        log::info!("not enabling review: {err:#}");
    }
    let workspace = helix_loader::find_workspace().0;
    let trust_full = cx
        .editor
        .workspace_trust
        .query(&workspace, TrustQuery::Git)
        .is_trusted();
    ensure!(
        cx.editor
            .diff_providers
            .has_staged_changes(&workspace, trust_full)?,
        "nothing is staged"
    );
    let subject = args.first().map(str::to_owned);
    hand_off(
        cx.editor,
        Some(ReviewRequest::CommitStaged { subject }),
        ":review-commit",
    )?;
    cx.editor.set_status("Handed off: commit staged hunks");
    Ok(())
}

/// Exports the handoff batch with `request`, refusing while a reviewed buffer is unsaved.
fn hand_off(
    editor: &mut Editor,
    request: Option<ReviewRequest>,
    command: &str,
) -> anyhow::Result<PathBuf> {
    let dir = editor
        .review_handoff
        .clone()
        .context("no review handoff directory, use :review-handoff <dir> first")?;
    let unsaved: Vec<_> = editor
        .documents()
        .filter(|doc| doc.review.is_some() && doc.is_modified())
        .map(|doc| doc.display_name().into_owned())
        .collect();
    ensure!(
        unsaved.is_empty(),
        "reviewed buffers have unsaved changes: {}; write or undo them before {command}",
        unsaved.join(", ")
    );
    if let Some(request) = request {
        editor.review_batch.set_request(request);
    }
    export_handoff(editor, &dir)
}

/// Starts reviewing the written document against the review baseline and exports a review batch
/// if a handoff directory is set.
pub(crate) fn document_written(editor: &mut Editor, doc_id: DocumentId) {
    if let Err(err) = editor.start_baseline_review(doc_id) {
        log::info!("not reviewing document {doc_id}: {err:#}");
    }
    let Some(dir) = editor.review_handoff.clone() else {
        return;
    };
    if doc!(editor, &doc_id).review.is_none() {
        return;
    }
    match export_handoff(editor, &dir) {
        Ok(path) => editor.set_status(format!("Exported review batch {}", path.display())),
        Err(err) => editor.set_error(format!("review handoff failed: {err:#}")),
    }
}

/// Exports every unmodified reviewed document and its comments to a new file in `dir`.
fn export_handoff(editor: &mut Editor, dir: &Path) -> anyhow::Result<PathBuf> {
    let mut modified = HashSet::new();
    let mut reviewed = Vec::new();
    for doc in editor.documents() {
        let Some(path) = doc.path() else {
            continue;
        };
        if doc.is_modified() {
            modified.insert(path.to_path_buf());
        } else if let Some(review) = &doc.review {
            reviewed.push((path.to_path_buf(), review.baseline().clone()));
        }
    }
    for (file, baseline) in reviewed {
        editor.review_batch.add_file(file, baseline);
    }

    let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let path = dir.join(format!("{millis}-{}.json", std::process::id()));
    let temp = path.with_extension("json.tmp");
    editor
        .review_batch
        .export(&temp, |file| !modified.contains(file))?;
    std::fs::rename(&temp, &path)
        .with_context(|| format!("failed to rename {} to {}", temp.display(), path.display()))?;
    Ok(path)
}

pub(super) fn review_files(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let commit = review_baseline_or_head(cx)?;
    let workspace = helix_loader::find_workspace().0;
    let trust_full = cx
        .editor
        .workspace_trust
        .query(&workspace, TrustQuery::Git)
        .is_trusted();
    let changes = cx
        .editor
        .diff_providers
        .get_changed_files_since(&workspace, &commit, trust_full)?;
    if changes.is_empty() {
        cx.editor
            .set_status(format!("no files changed since {}", &commit[..8]));
        return Ok(());
    }
    let files: Vec<_> = changes
        .into_iter()
        .map(|change| {
            let stats = file_stats(cx.editor, change.path(), &commit, trust_full);
            (change, stats)
        })
        .collect();
    let counted = files.iter().filter_map(|(_, stats)| *stats);
    let (added, removed, staged, total) = counted.fold((0, 0, 0, 0), |sum, stats| {
        (
            sum.0 + stats.added,
            sum.1 + stats.removed,
            sum.2 + stats.staged,
            sum.3 + stats.hunks,
        )
    });
    let status = format!(
        "{}, +{added} -{removed}, {staged}/{total} hunks staged",
        plural(files.len(), "file")
    );
    cx.jobs.callback(async move {
        let callback: job::Callback =
            Callback::EditorCompositor(Box::new(move |editor, compositor| {
                type Item = (FileChange, Option<FileStats>);
                let columns = [
                    ui::PickerColumn::new("change", |(change, _): &Item, _| {
                        match change {
                            FileChange::Untracked { .. } => "A",
                            FileChange::Deleted { .. } => "D",
                            _ => "M",
                        }
                        .into()
                    }),
                    ui::PickerColumn::new("path", |(change, _): &Item, workspace: &PathBuf| {
                        let path = change.path();
                        path.strip_prefix(workspace)
                            .unwrap_or(path)
                            .display()
                            .to_string()
                            .into()
                    }),
                    ui::PickerColumn::new("+/-", |(_, stats): &Item, _| {
                        match stats {
                            Some(stats) => match (stats.added, stats.removed) {
                                (added, 0) => format!("+{added}"),
                                (0, removed) => format!("-{removed}"),
                                (added, removed) => format!("+{added} -{removed}"),
                            },
                            None => "?".to_owned(),
                        }
                        .into()
                    }),
                    ui::PickerColumn::new("hunks", |(_, stats): &Item, _| {
                        match stats {
                            Some(stats) => format!("{}/{}", stats.staged, stats.hunks),
                            None => "?".to_owned(),
                        }
                        .into()
                    }),
                ];
                let picker = ui::Picker::new(
                    columns,
                    1,
                    files,
                    workspace,
                    |cx, (change, _): &Item, action| {
                        let doc_id = match cx.editor.open(change.path(), action) {
                            Ok(doc_id) => doc_id,
                            Err(err) => {
                                cx.editor.set_error(format!("{err:#}"));
                                return;
                            }
                        };
                        let view = view_mut!(cx.editor);
                        let doc = doc_mut!(cx.editor, &doc_id);
                        let Some(line) = doc.review.as_ref().and_then(Review::first_change_line)
                        else {
                            return;
                        };
                        let text = doc.text().slice(..);
                        let pos = text.line_to_char(line.min(text.len_lines() - 1));
                        doc.set_selection(view.id, Selection::point(pos));
                        align_view(doc, view, Align::Center);
                    },
                )
                .with_preview(|editor, (change, _): &Item| {
                    let path = change.path();
                    let line = editor
                        .document_by_path(path)
                        .and_then(|doc| doc.review.as_ref()?.first_change_line());
                    Some((path.into(), line.map(|line| (line, line))))
                });
                compositor.push(Box::new(overlaid(picker)));
                editor.set_status(status);
            }));
        Ok(callback)
    });
    Ok(())
}

pub(super) fn review_diff(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let commit = review_baseline_or_head(cx)?;
    let (diff, files) = review_diff_text(cx.editor, &commit)?;
    if files == 0 {
        cx.editor
            .set_status(format!("no files changed since {}", &commit[..8]));
        return Ok(());
    }

    // Reuse the scratch buffer of an earlier :review-diff while it is open.
    let doc_id = match cx
        .editor
        .review_diff
        .filter(|doc_id| cx.editor.documents.contains_key(doc_id))
    {
        Some(doc_id) => {
            cx.editor.switch(doc_id, Action::Replace);
            doc_id
        }
        None => {
            let doc_id = cx.editor.new_file(Action::Replace);
            cx.editor.review_diff = Some(doc_id);
            doc_id
        }
    };
    fill_review_diff(cx.editor, doc_id, diff)?;
    let scrolloff = cx.editor.config().scrolloff;
    let (view, doc) = current!(cx.editor);
    view.ensure_cursor_in_view(doc, scrolloff);
    cx.editor.set_status(format!(
        "Diff of {} against {}",
        plural(files, "changed file"),
        &commit[..8]
    ));
    Ok(())
}

/// The `:review-diff` text of every workspace file changed since `commit`, and the number of
/// changed files.
fn review_diff_text(editor: &Editor, commit: &str) -> anyhow::Result<(String, usize)> {
    let workspace = helix_loader::find_workspace().0;
    let trust_full = editor
        .workspace_trust
        .query(&workspace, TrustQuery::Git)
        .is_trusted();
    let changes = editor
        .diff_providers
        .get_changed_files_since(&workspace, commit, trust_full)?;
    let mut diff = String::new();
    for change in &changes {
        let path = change.path();
        let name = path.strip_prefix(&workspace).unwrap_or(path).display();
        let before = match editor
            .diff_providers
            .get_commit_base(path, commit, trust_full)
        {
            Ok(base) => String::from_utf8_lossy(&base.data).into_owned(),
            Err(err) => {
                diff.push_str(&format!("{name}: not diffed: {err:#}\n"));
                continue;
            }
        };
        let after = match editor.document_by_path(path) {
            Some(doc) if !doc.is_modified() => doc.text().to_string(),
            _ => std::fs::read(path)
                .map(|data| String::from_utf8_lossy(&data).into_owned())
                .unwrap_or_default(),
        };
        if [&before, &after]
            .iter()
            .any(|text| text.len() > Review::MAX_BYTES || text.lines().count() > Review::MAX_LINES)
        {
            diff.push_str(&format!(
                "{name}: over the review size limits, not diffed\n"
            ));
            continue;
        }
        let mut hunks = helix_vcs::unified_diff(&before, &after);
        if let Ok((index, _)) = editor.diff_providers.get_index_base(path, trust_full) {
            hunks = mark_staged_hunks(&hunks, &helix_vcs::line_hunks(&index, after.as_bytes()));
        }
        if !hunks.is_empty() {
            // Bare workspace-relative paths, so `gf` opens the file.
            diff.push_str(&format!("--- {name}\n+++ {name}\n{hunks}"));
        }
    }
    Ok((diff, changes.len()))
}

/// Appends " (staged)" to the `@@` header of every hunk in `hunks` (unified diff text) whose
/// changes `unstaged`, the index-to-worktree hunks, no longer touch.
fn mark_staged_hunks(hunks: &str, unstaged: &[helix_vcs::Hunk]) -> String {
    let mut marked = String::with_capacity(hunks.len());
    for line in hunks.split_inclusive('\n') {
        let current = line
            .strip_prefix("@@ ")
            .and_then(|header| header.split_whitespace().nth(1))
            .and_then(|range| range.strip_prefix('+'))
            .and_then(|range| {
                let (start, len) = range.split_once(',').unwrap_or((range, "1"));
                Some((start.parse::<usize>().ok()?, len.parse::<usize>().ok()?))
            });
        match current {
            Some((start, len)) => {
                // Hunk headers count from 1, or name line 0 for an empty side.
                let lines = start.saturating_sub(1)..start.saturating_sub(1) + len;
                let staged = helix_vcs::find_hunk(unstaged, &lines).is_none();
                let line = line.trim_end_matches('\n');
                marked.push_str(line);
                if staged {
                    marked.push_str(" (staged)");
                }
                marked.push('\n');
            }
            None => marked.push_str(line),
        }
    }
    marked
}

/// Replaces the text of the `:review-diff` buffer `doc_id` with `diff`, keeping the cursor line
/// of every view of it within the new text.
fn fill_review_diff(editor: &mut Editor, doc_id: DocumentId, diff: String) -> anyhow::Result<()> {
    let loader = editor.syn_loader.load();
    let current_view = view!(editor).id;
    let doc = doc_mut!(editor, &doc_id);
    let mut view_ids: Vec<_> = doc.selections().keys().copied().collect();
    if view_ids.is_empty() {
        doc.ensure_view_init(current_view);
        view_ids.push(current_view);
    }
    let text = doc.text().slice(..);
    let lines: Vec<_> = view_ids
        .iter()
        .map(|&view_id| doc.selection(view_id).primary().cursor_line(text))
        .collect();
    let transaction = Transaction::change(
        doc.text(),
        [(0, doc.text().len_chars(), Some(diff.into()))].into_iter(),
    );
    doc.apply(&transaction, view_ids[0]);
    doc.append_changes_to_history(view_mut!(editor, view_ids[0]));
    let doc = doc_mut!(editor, &doc_id);
    for (view_id, line) in view_ids.into_iter().zip(lines) {
        let text = doc.text().slice(..);
        let pos = text.line_to_char(line.min(text.len_lines() - 1));
        doc.set_selection(view_id, Selection::point(pos));
    }
    // Generated text: nothing to save, so quitting and auto-save leave it alone.
    doc.reset_modified();
    doc.readonly = true;
    doc.set_language_by_language_id("diff", &loader)?;
    Ok(())
}

pub(super) fn review_stage(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    ensure_review(cx)?;
    change_staging(cx.editor, Staging::Stage)
}

pub(super) fn review_unstage(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    ensure_review(cx)?;
    change_staging(cx.editor, Staging::Unstage)
}

pub(super) fn review_toggle_stage(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    ensure_review(cx)?;
    change_staging(cx.editor, Staging::Toggle)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Staging {
    Stage,
    Unstage,
    Toggle,
}

/// Stages or unstages the hunk under the cursor, see `:review-stage` and `:review-unstage`.
fn change_staging(editor: &mut Editor, staging: Staging) -> anyhow::Result<()> {
    let (path, target) = stage_target(editor)?;
    if let Some(doc) = editor.document_by_path(&path) {
        // The index is staged from the file on disk, which must be what the buffer shows.
        ensure!(
            !doc.is_modified(),
            "{} has unsaved changes; write or undo them before staging",
            doc.display_name()
        );
        ensure!(
            !doc.is_modified_on_disk().unwrap_or(false),
            "{} changed on disk; :reload it before staging",
            doc.display_name()
        );
    }
    let trust_full = editor
        .workspace_trust
        .query(&helix_loader::find_workspace().0, TrustQuery::Git)
        .is_trusted();
    let (index, expected) = editor.diff_providers.get_index_base(&path, trust_full)?;
    let worktree = std::fs::read(&path)?;
    let base = staging_base(editor, &path, &index, trust_full);
    let unstaged = helix_vcs::line_hunks(&index, &worktree);

    let (content, verb) = match helix_vcs::find_hunk(&unstaged, &target) {
        Some(hunk) if staging != Staging::Unstage => {
            (helix_vcs::apply_hunk(&index, &worktree, hunk), "Staged")
        }
        Some(_) => bail!("nothing staged here"),
        None if staging == Staging::Stage => bail!("nothing to stage here"),
        None => {
            // The target is outside every unstaged hunk, so it maps onto the index unchanged
            // but for the line count differences of the unstaged hunks before it.
            let target = worktree_to_index_lines(&unstaged, &target);
            let staged = helix_vcs::line_hunks(&base, &index);
            let hunk = helix_vcs::find_hunk(&staged, &target).context(match staging {
                Staging::Unstage => "nothing staged here",
                _ => "nothing to stage here",
            })?;
            let revert = helix_vcs::Hunk {
                before: hunk.after.clone(),
                after: hunk.before.clone(),
            };
            (helix_vcs::apply_hunk(&index, &base, &revert), "Unstaged")
        }
    };
    editor
        .diff_providers
        .update_index_entry(&path, &content, &expected, trust_full)?;

    let (count, total) = helix_vcs::staged_hunk_count(&base, &content, &worktree);
    if let Some(doc) = editor.document_by_path_mut(&path) {
        let text = doc.text().clone();
        if let Some(review) = doc.review.as_mut() {
            review.set_index(&content, &text);
        }
    }
    refresh_review_diff(editor);
    let name = path
        .strip_prefix(helix_loader::find_workspace().0)
        .unwrap_or(&path)
        .display()
        .to_string();
    editor.set_status(format!("{verb} hunk; {count} of {total} staged in {name}"));
    Ok(())
}

/// The content the hunks of `path` are counted and unstaged against: its open review's baseline,
/// else the editor-wide baseline commit, else the index itself.
fn staging_base(editor: &Editor, path: &Path, index: &[u8], trust_full: bool) -> Vec<u8> {
    match editor
        .document_by_path(path)
        .and_then(|doc| doc.review.as_ref())
    {
        Some(review) => review.snapshot().to_string().into_bytes(),
        None => editor
            .review_baseline
            .as_deref()
            .and_then(|commit| {
                editor
                    .diff_providers
                    .get_commit_base(path, commit, trust_full)
                    .ok()
            })
            .map(|base| base.data)
            .unwrap_or_else(|| index.to_vec()),
    }
}

/// Maps worktree `lines` outside every hunk of `unstaged` (index to worktree) to index lines.
fn worktree_to_index_lines(
    unstaged: &[helix_vcs::Hunk],
    lines: &ops::Range<usize>,
) -> ops::Range<usize> {
    let map = |line: usize| {
        let shift: isize = unstaged
            .iter()
            .filter(|hunk| hunk.after.end as usize <= line)
            .map(|hunk| hunk.after.len() as isize - hunk.before.len() as isize)
            .sum();
        (line as isize - shift).max(0) as usize
    };
    map(lines.start)..map(lines.end)
}

/// Line and hunk counts of a file's change since the review baseline, for `:review-files`.
#[derive(Clone, Copy)]
struct FileStats {
    added: usize,
    removed: usize,
    /// Hunks the git index already holds in full.
    staged: usize,
    hunks: usize,
}

/// The change of `path` from `commit` to the worktree (or its unmodified open buffer), like
/// `:review-diff` computes it, or `None` if the file is over the review size limits or its
/// baseline cannot be read.
fn file_stats(editor: &Editor, path: &Path, commit: &str, trust_full: bool) -> Option<FileStats> {
    let before = editor
        .diff_providers
        .get_commit_base(path, commit, trust_full)
        .ok()?
        .data;
    let after = match editor.document_by_path(path) {
        Some(doc) if !doc.is_modified() => doc.text().to_string().into_bytes(),
        _ => std::fs::read(path).unwrap_or_default(),
    };
    let lines = |text: &[u8]| text.split(|&byte| byte == b'\n').count();
    if [&before, &after]
        .iter()
        .any(|text| text.len() > Review::MAX_BYTES || lines(text) > Review::MAX_LINES)
    {
        return None;
    }
    let hunks = helix_vcs::line_hunks(&before, &after);
    let staged = match editor.diff_providers.get_index_base(path, trust_full) {
        Ok((index, _)) => helix_vcs::staged_hunk_count(&before, &index, &after).0,
        Err(_) => 0,
    };
    Some(FileStats {
        added: hunks.iter().map(|hunk| hunk.after.len()).sum(),
        removed: hunks.iter().map(|hunk| hunk.before.len()).sum(),
        staged,
        hunks: hunks.len(),
    })
}

/// Regenerates the open `:review-diff` buffer, if any, against the editor-wide baseline.
fn refresh_review_diff(editor: &mut Editor) {
    let (Some(doc_id), Some(commit)) = (
        editor
            .review_diff
            .filter(|doc_id| editor.documents.contains_key(doc_id)),
        editor.review_baseline.clone(),
    ) else {
        return;
    };
    if let Err(err) = review_diff_text(editor, &commit)
        .and_then(|(diff, _)| fill_review_diff(editor, doc_id, diff))
    {
        log::info!("not refreshing the review diff: {err:#}");
    }
}

/// Toggles the staging of the review hunk under the cursor when the buffer is reviewed or is the
/// `:review-diff` buffer, and does nothing otherwise, so it can own a key that is otherwise free.
pub(crate) fn review_stage_hunk(cx: &mut Context) {
    let doc = doc!(cx.editor);
    if doc.review.is_none() && cx.editor.review_diff != Some(doc.id()) {
        // Outside a review, Enter only takes over on an unstaged change of a workspace file,
        // and then enables the review first so the same press stages.
        if !on_unstaged_change(cx.editor) {
            return;
        }
        let mut cx = compositor::Context {
            editor: cx.editor,
            jobs: cx.jobs,
            scroll: None,
        };
        if let Err(err) = ensure_review(&mut cx) {
            cx.editor.set_error(format!("{err:#}"));
            return;
        }
    }
    if let Err(err) = change_staging(cx.editor, Staging::Toggle) {
        cx.editor.set_error(format!("{err:#}"));
    }
}

/// Whether the cursor line of the current buffer, a workspace file whose review was not closed,
/// is part of a change that is not staged in the git index.
fn on_unstaged_change(editor: &Editor) -> bool {
    let (view, doc) = current_ref!(editor);
    let workspace = helix_loader::find_workspace().0;
    let Some(path) = doc.path().filter(|path| path.starts_with(&workspace)) else {
        return false;
    };
    if doc.review_opt_out.is_some() && doc.review_opt_out == editor.review_baseline {
        return false;
    }
    let trust_full = editor
        .workspace_trust
        .query(&workspace, TrustQuery::Git)
        .is_trusted();
    let (Ok((index, _)), Ok(worktree)) = (
        editor.diff_providers.get_index_base(path, trust_full),
        std::fs::read(path),
    ) else {
        return false;
    };
    let line = doc
        .selection(view.id)
        .primary()
        .cursor_line(doc.text().slice(..));
    let hunks = helix_vcs::line_hunks(&index, &worktree);
    helix_vcs::find_hunk(&hunks, &(line..line + 1)).is_some()
}

/// The file and worktree line range `:review-stage` stages a hunk of: the hunk under the cursor of
/// the `:review-diff` buffer, the focused old text's block, or the cursor line of a reviewed buffer.
fn stage_target(editor: &Editor) -> anyhow::Result<(PathBuf, ops::Range<usize>)> {
    let (view, doc) = current_ref!(editor);
    let text = doc.text().slice(..);
    let cursor = doc.selection(view.id).primary().cursor_line(text);
    if editor.review_diff == Some(doc.id()) {
        let lines: Vec<String> = text
            .lines()
            .map(|line| line.to_string().trim_end_matches(['\n', '\r']).to_owned())
            .collect();
        let (path, line) = diff_location(&lines, cursor).context("nothing to stage here")?;
        let path = helix_loader::find_workspace().0.join(path);
        return Ok((path, line - 1..line));
    }
    let review = doc
        .review
        .as_ref()
        .context("not reviewing this buffer, use :review first")?;
    ensure!(!review.is_suspended(), Review::LIMIT_MESSAGE);
    let path = doc.path().context("buffer has no path")?.to_path_buf();
    let lines = match review.focused_block_index(view.id) {
        Some(idx) => review.blocks()[idx].current.clone(),
        None => cursor..cursor + 1,
    };
    Ok((path, lines))
}

pub(super) fn review_diff_open(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let (view, doc) = current_ref!(cx.editor);
    ensure!(
        cx.editor.review_diff == Some(doc.id()),
        "not the :review-diff buffer, use :review-diff first"
    );
    let text = doc.text().slice(..);
    let cursor = doc.selection(view.id).primary().cursor_line(text);
    let lines: Vec<String> = text
        .lines()
        .map(|line| line.to_string().trim_end_matches(['\n', '\r']).to_owned())
        .collect();
    let (path, line) =
        diff_location(&lines, cursor).context("the cursor is not inside a hunk of the diff")?;

    let path = helix_loader::find_workspace().0.join(path);
    let doc_id = cx.editor.open(&path, Action::Replace)?;
    let view = view_mut!(cx.editor);
    let doc = doc_mut!(cx.editor, &doc_id);
    let text = doc.text().slice(..);
    let pos = text.line_to_char((line - 1).min(text.len_lines() - 1));
    doc.set_selection(view.id, Selection::point(pos));
    align_view(doc, view, Align::Center);
    Ok(())
}

/// The file and 1-based current line that line `cursor` of a `:review-diff` buffer shows: the
/// first line on a file header, else the current line of the enclosing hunk at the cursor (the
/// next one on a removed line).
fn diff_location(lines: &[String], cursor: usize) -> Option<(&str, usize)> {
    let is_file_header = |idx: usize| {
        lines[idx].starts_with("--- ")
            && lines
                .get(idx + 1)
                .is_some_and(|next| next.starts_with("+++ "))
    };
    if is_file_header(cursor) {
        return Some((&lines[cursor][4..], 1));
    }
    if cursor > 0 && is_file_header(cursor - 1) {
        return Some((&lines[cursor][4..], 1));
    }
    let header = (0..=cursor)
        .rev()
        .find(|&idx| lines[idx].starts_with("@@ "))?;
    // Hunk lines start with ' ', '+' or '-'; anything else ends the hunk.
    let body = &lines[(header + 1).min(cursor)..cursor];
    if cursor > header && !lines[cursor].starts_with([' ', '+', '-'])
        || !body.iter().all(|line| line.starts_with([' ', '+', '-']))
    {
        return None;
    }
    let start: usize = lines[header]
        .split_whitespace()
        .nth(2)?
        .strip_prefix('+')?
        .split(',')
        .next()?
        .parse()
        .ok()?;
    let line = start + body.iter().filter(|line| !line.starts_with('-')).count();
    let path = (0..header)
        .rev()
        .find(|&idx| is_file_header(idx))
        .map(|idx| &lines[idx + 1][4..])?;
    Some((path, line))
}

/// Runs the static command `name` on the focused old text. Returns `false` if `name` is not
/// allowed there; only selection, navigation and yank commands are.
pub(crate) fn old_focus_command(cx: &mut Context, name: &str) -> bool {
    if name == "review_stage_hunk" {
        review_stage_hunk(cx);
        return true;
    }
    let count = cx.count();
    let scrolloff = cx.editor.config().scrolloff;
    let register = cx
        .register
        .unwrap_or(cx.editor.config().default_yank_register);
    let (view, doc) = current!(cx.editor);
    let view_id = view.id;
    let Some(review) = doc.review.as_mut() else {
        return false;
    };
    let Some(current) = review.focused_block_index(view_id) else {
        return false;
    };

    match name {
        "normal_mode" => {
            let line = review.blocks()[current].adjacent_line();
            review.clear_focus(view_id);
            let text = doc.text().slice(..);
            let pos = text.line_to_char(line.min(text.len_lines() - 1));
            doc.set_selection(view_id, Selection::point(pos));
            cx.editor.set_status("Returned to the current text");
        }
        "select_mode" => review.update_focus(view_id, |_, focus, _| OldFocus {
            extend: !focus.extend,
            ..focus
        }),
        "yank" | "yank_to_clipboard" | "yank_main_selection_to_clipboard" => {
            let register = if name == "yank" { register } else { '+' };
            let range = review.focus(view_id).unwrap().range;
            let text = range.fragment(review.snapshot().slice(..)).into_owned();
            match cx.editor.registers.write(register, vec![text]) {
                Ok(()) => cx
                    .editor
                    .set_status(format!("yanked old text to register {register}")),
                Err(err) => cx.editor.set_error(err.to_string()),
            }
        }
        "goto_next_change" | "goto_prev_change" | "goto_first_change" | "goto_last_change" => {
            let last = review.blocks().len() - 1;
            let idx = match name {
                "goto_next_change" => (current + count).min(last),
                "goto_prev_change" => current.saturating_sub(count),
                "goto_first_change" => 0,
                _ => last,
            };
            review.focus_block(view_id, idx);
            view.center_review_block_in_view(doc, scrolloff);
        }
        _ => {
            let (extend, motion) = match name.strip_prefix("extend_") {
                Some(motion) => (true, motion),
                None => (false, name.strip_prefix("move_").unwrap_or(name)),
            };
            let mut allowed = true;
            review.update_focus(view_id, |text, focus, block| {
                match move_old_selection(
                    motion,
                    text,
                    focus.range,
                    count,
                    extend || focus.extend,
                    block,
                ) {
                    Some(range) => OldFocus { range, ..focus },
                    None => {
                        allowed = false;
                        focus
                    }
                }
            });
            return allowed;
        }
    }
    true
}

/// Applies the selection command `motion` (without its `move_`/`extend_` prefix) to `range` in
/// the old text, or returns `None` if it isn't a supported selection command.
fn move_old_selection(
    motion: &str,
    text: RopeSlice,
    range: Range,
    count: usize,
    extend: bool,
    block: ops::Range<usize>,
) -> Option<Range> {
    use helix_core::movement::{self, Movement};

    let behaviour = if extend {
        Movement::Extend
    } else {
        Movement::Move
    };
    let text_fmt = TextFormat::default();
    let mut annotations = TextAnnotations::default();
    let word = |motion: fn(RopeSlice, Range, usize) -> Range| {
        let word = motion(text, range, count);
        if extend {
            range.put_cursor(text, word.cursor(text), true)
        } else {
            word
        }
    };
    let (first_line, last_line) = range.line_range(text);
    let line = range.cursor_line(text);
    let line_start = text.line_to_char(line);
    let line_after = |line: usize| text.line_to_char((line + 1).min(text.len_lines()));

    Some(match motion {
        "char_left" => movement::move_horizontally(
            text,
            range,
            Direction::Backward,
            count,
            behaviour,
            &text_fmt,
            &mut annotations,
        ),
        "char_right" => movement::move_horizontally(
            text,
            range,
            Direction::Forward,
            count,
            behaviour,
            &text_fmt,
            &mut annotations,
        ),
        "line_up" | "visual_line_up" | "line_down" | "visual_line_down" => {
            let direction = match motion {
                "line_up" | "visual_line_up" => Direction::Backward,
                _ => Direction::Forward,
            };
            let moved = movement::move_vertically(
                text,
                range,
                direction,
                count,
                behaviour,
                &text_fmt,
                &mut annotations,
            );
            // Clamping a motion past the block would only slide the cursor along its edge line.
            if block.contains(&moved.cursor(text)) {
                moved
            } else {
                range
            }
        }
        "next_word_start" => word(movement::move_next_word_start),
        "prev_word_start" => word(movement::move_prev_word_start),
        "next_word_end" => word(movement::move_next_word_end),
        "next_long_word_start" => word(movement::move_next_long_word_start),
        "prev_long_word_start" => word(movement::move_prev_long_word_start),
        "next_long_word_end" => word(movement::move_next_long_word_end),
        "goto_line_start" => range.put_cursor(text, line_start, extend),
        "goto_line_end" => {
            let end = line_end_char_index(&text, line);
            let end = if end > line_start {
                prev_grapheme_boundary(text, end)
            } else {
                end
            };
            range.put_cursor(text, end, extend)
        }
        "goto_file_start" => range.put_cursor(text, block.start, extend),
        "goto_last_line" | "goto_file_end" => {
            let last = text.line_to_char(text.char_to_line(block.end - 1));
            range.put_cursor(text, last, extend)
        }
        "line_below" => {
            let start = text.line_to_char(first_line);
            let end = line_after(last_line);
            if range.from() == start && range.to() == end {
                Range::new(start, line_after(last_line + count))
            } else {
                Range::new(start, line_after(last_line + count - 1))
            }
        }
        "to_line_bounds" => Range::new(text.line_to_char(first_line), line_after(last_line))
            .with_direction(range.direction()),
        "collapse_selection" => Range::point(range.cursor(text)),
        "flip_selections" => range.flip(),
        "select_all" => Range::new(block.start, block.end),
        _ => return None,
    })
}
