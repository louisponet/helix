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

/// Typable commands allowed while old text is focused. None of them modify a document.
const OLD_FOCUS_TYPABLE: &[&str] = &[
    "review-comment",
    "review-export",
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
    let status = format!(
        "Reviewing against the {}: {} block(s) of old text, :review-old to inspect them",
        review.baseline(),
        review.blocks().len()
    );
    doc.review = Some(review);
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
    let (view, doc) = current!(cx.editor);
    if doc.review.take().is_none() {
        bail!("not reviewing this buffer");
    }
    view.ensure_cursor_in_view(doc, scrolloff);
    cx.editor.set_status("Review closed");
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
    view.ensure_review_focus_in_view(doc, scrolloff);
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
        "{pending} pending review comment(s), :review-export <path> writes them"
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
        "Exported {comments} comment(s) on {files} file(s) to {}",
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
        for doc in cx.editor.documents_mut() {
            doc.review = None;
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
    let (mut reviewed, mut unsaved, mut failed) = (0, 0, 0);
    for doc_id in doc_ids {
        let doc = doc!(cx.editor, &doc_id);
        if doc.is_modified() {
            unsaved += 1;
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
            doc.review = Some(doc.load_review(
                &cx.editor.diff_providers,
                trust_full,
                revision.as_deref(),
            )?);
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
    cx.editor.set_status(format!(
        "Reviewing {reviewed} buffer(s) against git commit {}; {unsaved} unsaved and {failed} unreviewable buffer(s) skipped",
        &commit[..8]
    ));
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
                        if let Err(err) = cx.editor.open(change.path(), action) {
                            cx.editor.set_error(format!("{err:#}"));
                        }
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
            review.clear_focus(view_id);
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
        "line_up" | "visual_line_up" => movement::move_vertically(
            text,
            range,
            Direction::Backward,
            count,
            behaviour,
            &text_fmt,
            &mut annotations,
        ),
        "line_down" | "visual_line_down" => movement::move_vertically(
            text,
            range,
            Direction::Forward,
            count,
            behaviour,
            &text_fmt,
            &mut annotations,
        ),
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
