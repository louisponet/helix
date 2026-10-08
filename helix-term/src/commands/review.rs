//! Inline review of a buffer against an index or commit snapshot.

use std::collections::HashSet;
use std::ops;
use std::time::{SystemTime, UNIX_EPOCH};

use helix_core::command_line::Args;
use helix_core::doc_formatter::TextFormat;
use helix_core::graphemes::prev_grapheme_boundary;
use helix_core::line_ending::line_end_char_index;
use helix_core::text_annotations::TextAnnotations;
use helix_view::review::{OldFocus, Review, ReviewComment, ReviewSide};

use helix_loader::workspace_trust::TrustQuery;
use helix_vcs::FileChange;

use super::typed::{doc_trust_full, reload, reload_document};
use super::*;

const OLD_FOCUS_HELP: &str = "Old text (read-only): move, select, y yank, ]g/[g next/prev block, :review-comment <text>, Esc returns";

/// Typable commands allowed while old text is focused. Only `review-revert` modifies a document,
/// and it leaves the old text first.
const OLD_FOCUS_TYPABLE: &[&str] = &[
    "review-revert",
    "review-comment",
    "review-comments",
    "review-uncomment",
    "review-export",
    "review-send",
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
    start_review(cx, args.get(0))
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
        None => review.hunk_at_line(line).context("no changed lines here")?,
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

    let workspace = helix_loader::find_workspace().0;
    let trust_full = cx
        .editor
        .workspace_trust
        .query(&workspace, TrustQuery::Git)
        .is_trusted();
    let commit = cx
        .editor
        .diff_providers
        .resolve_commit(&workspace, &args[0], trust_full)?;
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
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let dir = cx
        .editor
        .review_handoff
        .clone()
        .context("no review handoff directory, use :review-handoff <dir> first")?;
    let unsaved: Vec<_> = cx
        .editor
        .documents()
        .filter(|doc| doc.review.is_some() && doc.is_modified())
        .map(|doc| doc.display_name().into_owned())
        .collect();
    ensure!(
        unsaved.is_empty(),
        "reviewed buffers have unsaved changes: {}; write or undo them before :review-send",
        unsaved.join(", ")
    );
    let path = export_handoff(cx.editor, &dir)?;
    cx.editor
        .set_status(format!("Exported review batch {}", path.display()));
    Ok(())
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

    let commit = cx
        .editor
        .review_baseline
        .clone()
        .context("no review baseline, use :review-baseline first")?;
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
    cx.jobs.callback(async move {
        let callback: job::Callback =
            Callback::EditorCompositor(Box::new(move |_editor, compositor| {
                let columns = [
                    ui::PickerColumn::new("change", |change: &FileChange, _| {
                        match change {
                            FileChange::Untracked { .. } => "A",
                            FileChange::Deleted { .. } => "D",
                            _ => "M",
                        }
                        .into()
                    }),
                    ui::PickerColumn::new("path", |change: &FileChange, workspace: &PathBuf| {
                        let path = change.path();
                        path.strip_prefix(workspace)
                            .unwrap_or(path)
                            .display()
                            .to_string()
                            .into()
                    }),
                ];
                let picker = ui::Picker::new(
                    columns,
                    1,
                    changes,
                    workspace,
                    |cx, change: &FileChange, action| {
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
                .with_preview(|_editor, change| Some((change.path().into(), None)));
                compositor.push(Box::new(overlaid(picker)));
            }));
        Ok(callback)
    });
    Ok(())
}

/// Runs the static command `name` on the focused old text. Returns `false` if `name` is not
/// allowed there; only selection, navigation and yank commands are.
pub(crate) fn old_focus_command(cx: &mut Context, name: &str) -> bool {
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
