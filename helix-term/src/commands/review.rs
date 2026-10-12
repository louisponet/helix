//! Inline review of a buffer against an index or commit snapshot.

use std::collections::{HashMap, HashSet};
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
    "review-ask",
    "review-comments",
    "review-uncomment",
    "review-export",
    "review-send",
    "review-commit",
    "review-commit-all",
    "review-close",
    "review-old",
    "review-next",
    "review-prev",
];

/// Static review commands (the `G` menu) allowed while old text is focused; each runs a command
/// of [`OLD_FOCUS_TYPABLE`] or opens the prompt for one.
const OLD_FOCUS_STATIC: &[&str] = &[
    "review_old_text",
    "review_comment_prompt",
    "review_ask_prompt",
    "review_comments_picker",
    "review_uncomment_latest",
    "review_toggle_stage",
    "review_revert_change",
    "review_close_buffer",
    "review_commit_staged",
    "review_commit_all",
    "review_send_handoff",
    "review_next_change",
    "review_prev_change",
];

/// Runs the typable review command `command` without arguments from a key, showing its error.
pub(crate) fn run_typed(
    cx: &mut Context,
    command: fn(&mut compositor::Context, Args, PromptEvent) -> anyhow::Result<()>,
) {
    let mut cx = compositor::Context {
        editor: cx.editor,
        jobs: cx.jobs,
        scroll: None,
    };
    if let Err(err) = command(&mut cx, Args::default(), PromptEvent::Validate) {
        cx.editor.set_error(format!("{err:#}"));
    }
}

/// Jumps to the next (or previous) changed block `count` times from a key, across files.
pub(crate) fn run_jump(cx: &mut Context, forward: bool) {
    let count = cx.count();
    let mut cx = compositor::Context {
        editor: cx.editor,
        jobs: cx.jobs,
        scroll: None,
    };
    if let Err(err) = jump_change(&mut cx, forward, count) {
        cx.editor.set_error(format!("{err:#}"));
    }
}

pub(super) fn review_next(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    jump_change(cx, true, 1)
}

pub(super) fn review_prev(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    jump_change(cx, false, 1)
}

/// The workspace files changed since the review baseline, in `:review-files` order, pinning
/// `HEAD` as the baseline when none is set. Returns the baseline too.
fn changed_files(cx: &mut compositor::Context) -> anyhow::Result<(String, Vec<FileChange>)> {
    let commit = review_baseline_or_head(cx)?;
    let changes = changed_files_since(cx.editor, &commit)?;
    Ok((commit, changes))
}

/// The workspace files changed since `revision`, in `:review-files` order.
pub(super) fn changed_files_since(
    editor: &Editor,
    revision: &str,
) -> anyhow::Result<Vec<FileChange>> {
    let workspace = helix_loader::find_workspace().0;
    let trust_full = editor
        .workspace_trust
        .query(&workspace, TrustQuery::Git)
        .is_trusted();
    let commit = editor
        .diff_providers
        .resolve_commit(&workspace, revision, trust_full)?;
    editor
        .diff_providers
        .get_changed_files_since(&workspace, &commit, trust_full)
}

/// Moves to the next (or previous) changed block `count` times: within the current buffer, then
/// on to the first (or last) block of the next (or previous) changed file, wrapping around.
fn jump_change(cx: &mut compositor::Context, forward: bool, count: usize) -> anyhow::Result<()> {
    cx.editor.nav_mode = helix_view::editor::NavMode::Review;
    let (commit, changes) = changed_files(cx)?;
    // Deleted files have nothing to land on.
    let files: Vec<PathBuf> = changes
        .iter()
        .filter(|change| !matches!(change, FileChange::Deleted { .. }))
        .map(|change| change.path().to_path_buf())
        .collect();
    ensure!(!files.is_empty(), "no changes since {}", &commit[..8]);
    if let Err(err) = ensure_review(cx) {
        log::info!("not enabling review: {err:#}");
    }
    for _ in 0..count.max(1) {
        let (view, doc) = current_ref!(cx.editor);
        let cursor = doc
            .selection(view.id)
            .primary()
            .cursor_line(doc.text().slice(..));
        let hunks = doc
            .review
            .as_ref()
            .map(Review::uncommitted_hunk_lines)
            .unwrap_or_default();
        let here = if forward {
            hunks.iter().position(|lines| lines.start > cursor)
        } else {
            hunks.iter().rposition(|lines| lines.start < cursor)
        };
        if let Some(idx) = here {
            land_on_hunk(cx.editor, hunks[idx].start);
            continue;
        }
        // Past the last (or first) block: the next (or previous) file with an uncommitted hunk,
        // wrapping around.
        let current = doc
            .path()
            .and_then(|path| files.iter().position(|file| file == path));
        let len = files.len();
        let order: Vec<usize> = match (current, forward) {
            (Some(idx), true) => (1..=len).map(|step| (idx + step) % len).collect(),
            (Some(idx), false) => (1..=len).map(|step| (idx + len - step) % len).collect(),
            (None, true) => (0..len).collect(),
            (None, false) => (0..len).rev().collect(),
        };
        let trust_full = doc_trust_full(cx.editor);
        let mut landed = false;
        for next in order {
            let file = &files[next];
            // A saved file that matches HEAD has only committed hunks: skip it without opening.
            let all_committed = !cx
                .editor
                .document_by_path(file)
                .is_some_and(Document::is_modified)
                && cx
                    .editor
                    .diff_providers
                    .get_commit_base(file, "HEAD", trust_full)
                    .ok()
                    .zip(std::fs::read(file).ok())
                    .is_some_and(|(head, worktree)| head.data == worktree);
            if all_committed {
                continue;
            }
            cx.editor.open(file, Action::Replace)?;
            if let Err(err) = ensure_review(cx) {
                log::info!("not enabling review: {err:#}");
            }
            let hunks = doc!(cx.editor)
                .review
                .as_ref()
                .map(Review::uncommitted_hunk_lines)
                .unwrap_or_default();
            if let Some(target) = if forward { hunks.first() } else { hunks.last() } {
                land_on_hunk(cx.editor, target.start);
                landed = true;
                break;
            }
        }
        ensure!(landed, "no uncommitted changes since {}", &commit[..8]);
    }
    let (view, doc) = current_ref!(cx.editor);
    let cursor = doc
        .selection(view.id)
        .primary()
        .cursor_line(doc.text().slice(..));
    let hunks = doc
        .review
        .as_ref()
        .map(Review::uncommitted_hunk_lines)
        .unwrap_or_default();
    let name = doc
        .path()
        .map(|path| {
            path.strip_prefix(helix_loader::find_workspace().0)
                .unwrap_or(path)
                .display()
                .to_string()
        })
        .unwrap_or_else(|| doc.display_name().into_owned());
    match hunks.iter().position(|lines| lines.start == cursor) {
        Some(idx) => cx
            .editor
            .set_status(format!("{name}: hunk {} of {}", idx + 1, hunks.len())),
        None => cx.editor.set_status(format!("{name}: no hunks to show")),
    }
    Ok(())
}

/// Puts the cursor at the start of `line` (clamped to the buffer), centred, leaving old text.
fn land_on_hunk(editor: &mut Editor, line: usize) {
    let (view, doc) = current!(editor);
    if let Some(review) = doc.review.as_mut() {
        review.clear_focus(view.id);
    }
    let text = doc.text().slice(..);
    let pos = text.line_to_char(line.min(text.len_lines().saturating_sub(1)));
    push_jump(view, doc);
    doc.set_selection(view.id, Selection::point(pos));
    align_view(doc, view, Align::Center);
}

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
pub(super) fn plural(count: usize, noun: &str) -> String {
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

/// How long ago `time` (seconds since the epoch) was, as in `3 days ago`.
fn relative_age(time: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |now| now.as_secs() as i64);
    let secs = (now - time).max(0);
    let (count, unit) = match secs {
        0..60 => return "just now".to_owned(),
        60..3_600 => (secs / 60, "minute"),
        3_600..86_400 => (secs / 3_600, "hour"),
        86_400..2_592_000 => (secs / 86_400, "day"),
        2_592_000..31_536_000 => (secs / 2_592_000, "month"),
        _ => (secs / 31_536_000, "year"),
    };
    format!("{} ago", plural(count as usize, unit))
}

/// A row of the `:review-baseline-pick` picker.
struct BaselineEntry {
    commit: helix_vcs::CommitEntry,
    /// `●` for the current baseline, `base` for the merge base with the base branch, `pi` for the
    /// pi hook's baseline, `HEAD`.
    marker: String,
    /// How many commits it is behind `HEAD`.
    behind: usize,
}

pub(super) fn review_baseline_pick(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let workspace = helix_loader::find_workspace().0;
    let trust_full = cx
        .editor
        .workspace_trust
        .query(&workspace, TrustQuery::Git)
        .is_trusted();
    let log = cx
        .editor
        .diff_providers
        .first_parent_log(&workspace, 300, trust_full)?;
    let current = cx.editor.review_baseline.clone();
    let hook = cx.editor.review_hook_baseline.clone();
    let base = super::review_pr::base_merge_base(cx.editor);
    let entries: Vec<_> = log
        .into_iter()
        .enumerate()
        .map(|(behind, commit)| {
            let mut marks = Vec::new();
            if current.as_deref() == Some(commit.id.as_str()) {
                marks.push("●");
            }
            if base.as_deref() == Some(commit.id.as_str()) {
                marks.push("base");
            }
            if hook.as_deref() == Some(commit.id.as_str()) {
                marks.push("pi");
            }
            if behind == 0 {
                marks.push("HEAD");
            }
            BaselineEntry {
                marker: marks.join(" "),
                commit,
                behind,
            }
        })
        .collect();
    let selected = entries
        .iter()
        .position(|entry| current.as_deref() == Some(entry.commit.id.as_str()))
        .unwrap_or(0);
    cx.jobs.callback(async move {
        let callback: job::Callback =
            Callback::EditorCompositor(Box::new(move |_editor, compositor| {
                let columns = [
                    ui::PickerColumn::new("mark", |entry: &BaselineEntry, _| {
                        entry.marker.as_str().into()
                    }),
                    ui::PickerColumn::new("commit", |entry: &BaselineEntry, _| {
                        entry.commit.id[..8].into()
                    }),
                    ui::PickerColumn::new("age", |entry: &BaselineEntry, _| {
                        relative_age(entry.commit.time).into()
                    }),
                    ui::PickerColumn::new("author", |entry: &BaselineEntry, _| {
                        entry.commit.author.as_str().into()
                    }),
                    ui::PickerColumn::new("subject", |entry: &BaselineEntry, _| {
                        entry.commit.summary.as_str().into()
                    }),
                ];
                let picker = ui::Picker::new(columns, 4, entries, (), |cx, entry, _action| {
                    let line = format!("review-baseline {}", entry.commit.id);
                    cx.editor.clear_status();
                    let result = typed::execute_command_line(cx, &line, PromptEvent::Validate);
                    match result {
                        Ok(()) if !cx.editor.is_err() => cx.editor.set_status(format!(
                            "Baseline {} {} ({} behind HEAD)",
                            &entry.commit.id[..8],
                            entry.commit.summary,
                            entry.behind
                        )),
                        Ok(()) => (),
                        Err(err) => cx.editor.set_error(format!("{err:#}")),
                    }
                })
                .with_initial_cursor(selected as u32);
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
    if let Some(pr) = cx.editor.review_pr.clone() {
        super::review_pr::follow_merge_base(cx.editor)?;
        cx.jobs.add(super::review_pr::load_threads(pr));
    }
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

/// Outside a pull request review, hands every selection with `args` to the agent at once; while
/// reviewing a pull request, adds a pending pull request comment or thread reply.
pub(super) fn review_comment(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    if cx.editor.review_pr.is_none() {
        return send_comments(cx, &args[0]);
    }
    ensure_review(cx)?;
    let mut comment = selection_comments(cx.editor, &args[0], false)?.remove(0);
    // Pull request comments wait for :review-submit; on a thread's line they reply to it.
    let (view, doc) = current_ref!(cx.editor);
    let text = doc.text().slice(..);
    let cursor = doc.selection(view.id).primary().cursor_line(text);
    comment.reply_to = doc.review.as_ref().and_then(|review| {
        review
            .threads()
            .iter()
            .find(|thread| review.thread_anchor(thread, text.len_lines()).0 == cursor)
            .map(|thread| thread.root_id())
    });
    let reply = comment.reply_to.is_some();
    cx.editor.review_pr_comments.push(comment);
    let pending = cx.editor.review_pr_comments.len();
    cx.editor.set_status(format!(
        "{}{}, :review-submit posts them",
        if reply { "Reply added; " } else { "" },
        plural(pending, "pending pull request comment")
    ));
    Ok(())
}

/// Comments with `text` on the focused old text, or on the primary selection (every selection
/// range if `all_ranges`) of the current buffer.
fn selection_comments(
    editor: &Editor,
    text: &str,
    all_ranges: bool,
) -> anyhow::Result<Vec<ReviewComment>> {
    let (view, doc) = current_ref!(editor);
    let review = doc
        .review
        .as_ref()
        .context("not reviewing this buffer, use :review first")?;
    ensure!(!review.is_suspended(), Review::LIMIT_MESSAGE);
    let path = doc.path().context("buffer has no path")?.to_path_buf();
    if let Some((focus, idx)) = review
        .focus(view.id)
        .zip(review.focused_block_index(view.id))
    {
        return Ok(vec![ReviewComment::new(
            path,
            ReviewSide::Old,
            review.baseline(),
            review.snapshot().slice(..),
            focus.range,
            review.blocks()[idx].current.start,
            text.to_owned(),
        )]);
    }
    let doc_text = doc.text().slice(..);
    let selection = doc.selection(view.id);
    // The primary selection comes first.
    let mut ranges = vec![selection.primary()];
    if all_ranges {
        ranges.extend(
            (selection.iter().enumerate())
                .filter(|&(idx, _)| idx != selection.primary_index())
                .map(|(_, range)| *range),
        );
    }
    Ok(ranges
        .into_iter()
        .map(|range| ReviewComment {
            enclosing_text: enclosing_text(doc, range),
            ..ReviewComment::new(
                path.clone(),
                ReviewSide::Current,
                review.baseline(),
                doc_text,
                range,
                doc_text.char_to_line(range.from()),
                text.to_owned(),
            )
        })
        .collect())
}

/// The text of the smallest syntax node around `range` that is a function, type or similar item
/// (or, failing that, spans at least three lines), cut at 200 lines. `None` without a syntax tree.
fn enclosing_text(doc: &Document, range: Range) -> Option<String> {
    const ITEM_KINDS: &[&str] = &[
        "function", "method", "impl", "class", "struct", "enum", "trait", "module",
    ];
    const MAX_LINES: usize = 200;
    let text = doc.text().slice(..);
    let syntax = doc.syntax()?;
    let (from, to) = (
        text.char_to_byte(range.from()) as u32,
        text.char_to_byte(range.to()) as u32,
    );
    let first = syntax.named_descendant_for_byte_range(from, to)?;
    let ancestors: Vec<_> = std::iter::successors(Some(first), |node| node.parent()).collect();
    let line_span =
        |start: u32, end: u32| text.byte_to_line(end as usize) - text.byte_to_line(start as usize);
    let node = ancestors
        .iter()
        .find(|node| ITEM_KINDS.iter().any(|kind| node.kind().contains(kind)))
        .or_else(|| {
            ancestors
                .iter()
                .find(|node| line_span(node.start_byte(), node.end_byte()) >= 2)
        })?;
    let start = node.start_byte() as usize;
    let first_line = text.byte_to_line(start);
    let end = if line_span(node.start_byte(), node.end_byte()) >= MAX_LINES {
        text.line_to_byte(first_line + MAX_LINES)
    } else {
        node.end_byte() as usize
    };
    Some(text.byte_slice(start..end).to_string())
}

/// Comments with `text` on every selection (or the selected old text) and hands the review off
/// at once as a question, writing the buffer first if it is modified.
fn send_comments(cx: &mut compositor::Context, text: &str) -> anyhow::Result<()> {
    ensure!(
        cx.editor.review_handoff_dir().is_some(),
        "no review handoff directory, use :review-handoff <dir> first"
    );
    ensure!(doc!(cx.editor).path().is_some(), "buffer has no path");
    ensure_review(cx)?;
    let (target, message) = split_target(Some(text));
    let message = message.context("the comment is empty")?;
    let comments = selection_comments(cx.editor, message, true)?;
    let count = comments.len();
    for comment in comments {
        cx.editor.review_batch.add_comment(comment);
    }
    cx.editor.review_batch.set_request(ReviewRequest::Ask);
    if let Some(target) = target {
        cx.editor.review_batch.set_target(target);
    }
    if doc!(cx.editor).is_modified() {
        // The batch takes the saved file: it is exported once the write is done.
        cx.editor.review_send_on_write = Some(doc!(cx.editor).id());
        typed::write_impl(
            cx,
            None,
            typed::WriteOptions {
                force: false,
                auto_format: true,
                code_actions: true,
            },
        )?;
    } else {
        hand_off(cx.editor, None, None, ":review-comment")?;
    }
    cx.editor
        .set_status(format!("Sent {} to the agent", plural(count, "comment")));
    Ok(())
}

/// A row of the `:review-comments` picker: a pending comment or a pull request thread.
struct CommentEntry {
    kind: &'static str,
    author: String,
    path: PathBuf,
    /// The lines shown, such as `3`, `3-5` or `old 3`.
    lines: String,
    text: String,
    /// The 0-based document line to jump to.
    line: usize,
    /// The 0-based document lines to highlight in the preview.
    preview: (usize, usize),
}

/// The pending comments (for the pull request in PR mode) and the pull request's threads.
fn comment_entries(editor: &Editor) -> Vec<CommentEntry> {
    let pending = match editor.review_pr {
        Some(_) => &editor.review_pr_comments[..],
        None => editor.review_batch.comments(),
    };
    let mut entries: Vec<_> = pending
        .iter()
        .map(|comment| {
            let author = comment
                .reply_to
                .and_then(|root| {
                    editor
                        .review_pr_threads
                        .iter()
                        .find(|thread| thread.root_id() == root)
                })
                .and_then(|thread| thread.comments.first())
                .map_or_else(
                    || "you".to_owned(),
                    |root| format!("reply to {}", root.login),
                );
            let (lines, preview) = match comment.side {
                ReviewSide::Current => (
                    match comment.start_line == comment.end_line {
                        true => comment.start_line.to_string(),
                        false => format!("{}-{}", comment.start_line, comment.end_line),
                    },
                    (comment.start_line - 1, comment.end_line - 1),
                ),
                ReviewSide::Old => (
                    format!("old {}", comment.start_line),
                    (comment.current_line - 1, comment.current_line - 1),
                ),
            };
            CommentEntry {
                kind: "pending",
                author,
                path: comment.path.clone(),
                lines,
                text: comment.comment.clone(),
                line: comment.current_line - 1,
                preview,
            }
        })
        .collect();
    for thread in &editor.review_pr_threads {
        let anchor = editor
            .document_by_path(&thread.path)
            .and_then(|doc| {
                let review = doc.review.as_ref()?;
                Some(review.thread_anchor(thread, doc.text().len_lines()).0)
            })
            .unwrap_or_else(|| thread.line.or(thread.original_line).unwrap_or(1) - 1);
        let root = thread.comments.first();
        entries.push(CommentEntry {
            kind: "thread",
            author: root.map(|root| root.login.clone()).unwrap_or_default(),
            path: thread.path.clone(),
            lines: (anchor + 1).to_string(),
            text: root
                .and_then(|root| root.body.lines().next())
                .unwrap_or_default()
                .to_owned(),
            line: anchor,
            preview: (anchor, anchor),
        });
    }
    entries
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

    let entries = comment_entries(cx.editor);
    ensure!(!entries.is_empty(), "no pending review comments");
    let workspace = helix_loader::find_workspace().0;
    cx.jobs.callback(async move {
        let callback: job::Callback =
            Callback::EditorCompositor(Box::new(move |_editor, compositor| {
                let columns = [
                    ui::PickerColumn::new("kind", |entry: &CommentEntry, _| entry.kind.into()),
                    ui::PickerColumn::new("author", |entry: &CommentEntry, _| {
                        entry.author.as_str().into()
                    }),
                    ui::PickerColumn::new("place", |entry: &CommentEntry, workspace: &PathBuf| {
                        let path = entry.path.strip_prefix(workspace).unwrap_or(&entry.path);
                        format!("{}:{}", path.display(), entry.lines).into()
                    }),
                    ui::PickerColumn::new("comment", |entry: &CommentEntry, _| {
                        entry.text.as_str().into()
                    }),
                ];
                let picker =
                    ui::Picker::new(columns, 3, entries, workspace, |cx, entry, action| {
                        let doc_id = match cx.editor.open(&entry.path, action) {
                            Ok(id) => id,
                            Err(err) => {
                                cx.editor.set_error(format!(
                                    "failed to open {}: {err}",
                                    entry.path.display()
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
                        let line = entry.line.min(text.len_lines() - 1);
                        doc.set_selection(view.id, Selection::point(text.line_to_char(line)));
                        if action.align_view(view, doc_id) {
                            align_view(doc, view, Align::Center);
                        }
                    })
                    .with_preview(|_editor, entry| {
                        Some((entry.path.as_path().into(), Some(entry.preview)))
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
    let (removed, pending) = if cx.editor.review_pr.is_some() {
        let comments = &mut cx.editor.review_pr_comments;
        ensure!(!comments.is_empty(), "no pending review comments");
        let removed = count.min(comments.len());
        comments.truncate(comments.len() - removed);
        (removed, comments.len())
    } else {
        let batch = &mut cx.editor.review_batch;
        ensure!(!batch.comments().is_empty(), "no pending review comments");
        let removed = batch.remove_latest_comments(count);
        (removed, batch.comments().len())
    };
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

    let commit = if &args[0] != "clear" {
        // A stale commit (one a rebase rewrote, say) is never adopted as the baseline.
        let workspace = helix_loader::find_workspace().0;
        let trust_full = cx
            .editor
            .workspace_trust
            .query(&workspace, TrustQuery::Git)
            .is_trusted();
        let providers = &cx.editor.diff_providers;
        let commit = providers.resolve_commit(&workspace, &args[0], trust_full)?;
        ensure!(
            providers.is_in_head_history(&workspace, &commit, trust_full)?,
            "commit {} is not in this branch's history",
            &commit[..8]
        );
        if cx.editor.socket_command {
            cx.editor.review_hook_baseline = Some(commit.clone());
        }
        commit
    } else {
        String::new()
    };
    if cx.editor.review_pr.is_some() && &args[0] != "clear" {
        if cx.editor.socket_command {
            // The hook re-sending its baseline only refreshes the pull request's (or a stepped one).
            let current = cx
                .editor
                .review_baseline
                .clone()
                .context("no review baseline")?;
            return pin_review_baseline(cx.editor, &current);
        }
        // A baseline the user picks ends the pull request review.
        cx.editor.review_pr = None;
        cx.editor.review_user_pin = None;
        cx.editor.review_pr_threads.clear();
        cx.editor.share_review_threads();
    }
    if &args[0] == "clear" {
        cx.editor.review_baseline = None;
        cx.editor.review_user_pin = None;
        cx.editor.review_pr = None;
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
    if cx.editor.socket_command {
        // The hook never overrides a baseline the reviewer chose: it only refreshes it.
        if let Some(pin) = cx
            .editor
            .review_user_pin
            .clone()
            .filter(|pin| *pin != commit)
        {
            let current = cx.editor.review_baseline.clone().unwrap_or(pin);
            return pin_review_baseline(cx.editor, &current);
        }
        cx.editor.review_user_pin = None;
        return pin_review_baseline(cx.editor, &commit);
    }
    pin_review_baseline(cx.editor, &commit)?;
    set_user_pin(cx.editor, &commit);
    Ok(())
}

/// Records `commit`, just chosen by the reviewer, as pinned unless it is the hook's baseline.
fn set_user_pin(editor: &mut Editor, commit: &str) {
    editor.review_user_pin =
        (editor.review_hook_baseline.as_deref() != Some(commit)).then(|| commit.to_owned());
}

pub(super) fn review_older(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let base = review_baseline_or_head(cx)?;
    step_baseline(cx, &base, true)
}

pub(super) fn review_newer(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let base = cx
        .editor
        .review_baseline
        .clone()
        .context("no review baseline set")?;
    step_baseline(cx, &base, false)
}

/// Moves the editor-wide baseline one commit older or newer than `base`, pinned by the reviewer
/// unless it is the hook's baseline.
fn step_baseline(cx: &mut compositor::Context, base: &str, older: bool) -> anyhow::Result<()> {
    let workspace = helix_loader::find_workspace().0;
    let trust_full = cx
        .editor
        .workspace_trust
        .query(&workspace, TrustQuery::Git)
        .is_trusted();
    let (commit, behind) = cx
        .editor
        .diff_providers
        .step_commit(&workspace, base, older, trust_full)?;
    pin_review_baseline(cx.editor, &commit.id)?;
    set_user_pin(cx.editor, &commit.id);
    cx.editor.set_status(format!(
        "Baseline {} {} ({behind} behind HEAD)",
        &commit.id[..8],
        commit.summary
    ));
    Ok(())
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

/// Rereads the git index and `HEAD` content of `path` into its open review, so its hunks are marked
/// staged or committed as git has them now.
fn refresh_staging(editor: &mut Editor, path: &Path, trust_full: bool) {
    let Ok((index, _)) = editor.diff_providers.get_index_base(path, trust_full) else {
        return;
    };
    let head = editor
        .diff_providers
        .get_commit_base(path, "HEAD", trust_full)
        .ok();
    if let Some(doc) = editor.document_by_path_mut(path) {
        let text = doc.text().clone();
        if let Some(review) = doc.review.as_mut() {
            review.set_index(&index, head.as_ref().map(|head| &head.data[..]), &text);
        }
    }
}

/// The editor-wide review baseline, pinning `HEAD` like `:review-baseline HEAD` when none is set.
fn review_baseline_or_head(cx: &mut compositor::Context) -> anyhow::Result<String> {
    if cx.editor.review_baseline.is_none() {
        pin_review_baseline(cx.editor, "HEAD")
            .context("no review baseline, and HEAD cannot be resolved")?;
    }
    Ok(cx.editor.review_baseline.clone().unwrap())
}

/// Pins `revision` as the editor-wide review baseline and reviews the workspace buffers against it.
pub(super) fn pin_review_baseline(editor: &mut Editor, revision: &str) -> anyhow::Result<()> {
    let workspace = helix_loader::find_workspace().0;
    let trust_full = editor
        .workspace_trust
        .query(&workspace, TrustQuery::Git)
        .is_trusted();
    let commit = editor
        .diff_providers
        .resolve_commit(&workspace, revision, trust_full)?;
    let refresh = editor.review_baseline.as_ref() == Some(&commit);
    editor.review_baseline = Some(commit.clone());

    let doc_ids: Vec<_> = editor
        .documents()
        .filter(|doc| doc.path().is_some_and(|path| path.starts_with(&workspace)))
        .map(Document::id)
        .collect();
    let (mut reviewed, mut unchanged, mut unsaved, mut failed, mut closed) = (0, 0, 0, 0, 0);
    let mut conflicts = Vec::new();
    for doc_id in doc_ids {
        let doc = doc!(editor, &doc_id);
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
            // The agent may have committed or staged since: the hunks' stages can still change.
            if let Some(path) = doc.path().map(Path::to_path_buf) {
                refresh_staging(editor, &path, trust_full);
            }
            unchanged += 1;
            continue;
        }
        let revision = match &doc.review {
            Some(review) if refresh => review.baseline().revision().map(str::to_owned),
            _ => Some(commit.clone()),
        };
        let result = reload_document(editor, doc_id).and_then(|()| {
            let doc = doc_mut!(editor, &doc_id);
            let trust_full = editor
                .workspace_trust
                .query(doc.workspace_root(), TrustQuery::Git)
                .is_trusted();
            let review =
                doc.load_review(&editor.diff_providers, trust_full, revision.as_deref())?;
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
    refresh_review_diff(editor);

    let scrolloff = editor.config().scrolloff;
    let (view, doc) = current!(editor);
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
    editor.set_status(status);
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
        "Review handoffs export review batches to {}",
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
    let (target, text) = split_target(args.first());
    let request = text.map(|text| ReviewRequest::Message {
        text: text.to_owned(),
    });
    let path = hand_off(cx.editor, request, target, ":review-send")?;
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
    if !cx
        .editor
        .diff_providers
        .has_staged_changes(&workspace, trust_full)?
    {
        let committed = doc!(cx.editor)
            .review
            .as_ref()
            .map_or(0, |review| review.committed_hunks());
        ensure!(committed > 0, "nothing is staged");
        let head = cx
            .editor
            .diff_providers
            .resolve_commit(&workspace, "HEAD", trust_full)?;
        bail!(
            "nothing is staged; {} already committed in {}",
            plural(committed, "hunk"),
            &head[..8]
        );
    }
    let (target, subject) = split_target(args.first());
    hand_off(
        cx.editor,
        Some(ReviewRequest::CommitStaged {
            subject: subject.map(str::to_owned),
        }),
        target,
        ":review-commit",
    )?;
    cx.editor.set_status("Handed off: commit staged hunks");
    Ok(())
}

/// Refuses `command` while a reviewed buffer has unsaved changes.
pub(super) fn ensure_reviews_saved(editor: &Editor, command: &str) -> anyhow::Result<()> {
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
    Ok(())
}

pub(super) fn review_commit_all(
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
    let (commit, changes) = changed_files(cx)?;
    ensure!(!changes.is_empty(), "no changes since {}", &commit[..8]);
    ensure_reviews_saved(cx.editor, ":review-commit-all")?;
    ensure!(
        cx.editor.review_handoff_dir().is_some(),
        "no review handoff directory, use :review-handoff <dir> first"
    );
    let workspace = helix_loader::find_workspace().0;
    let trust_full = cx
        .editor
        .workspace_trust
        .query(&workspace, TrustQuery::Git)
        .is_trusted();
    let paths: Vec<PathBuf> = changes
        .iter()
        .map(|change| change.path().to_path_buf())
        .collect();
    cx.editor
        .diff_providers
        .stage_paths(&workspace, &paths, trust_full)?;
    // Reviews mark staged hunks from the index they last saw.
    for path in &paths {
        refresh_staging(cx.editor, path, trust_full);
    }
    refresh_review_diff(cx.editor);
    let (target, subject) = split_target(args.first());
    hand_off(
        cx.editor,
        Some(ReviewRequest::CommitStaged {
            subject: subject.map(str::to_owned),
        }),
        target,
        ":review-commit-all",
    )?;
    cx.editor.set_status(format!(
        "Staged {}; handed off: commit staged hunks",
        plural(paths.len(), "file")
    ));
    Ok(())
}

/// Splits a leading `@label` naming the pi session to address off a command's text, returning
/// the label and the rest of the text, if any is left.
fn split_target(text: Option<&str>) -> (Option<String>, Option<&str>) {
    let Some(text) = text else {
        return (None, None);
    };
    let Some(rest) = text.strip_prefix('@') else {
        return (None, Some(text));
    };
    let (label, rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let rest = rest.trim_start();
    (
        Some(label.to_owned()).filter(|label| !label.is_empty()),
        Some(rest).filter(|rest| !rest.is_empty()),
    )
}

/// Exports the handoff batch with `request`, addressed to the pi session `target` if given,
/// refusing while a reviewed buffer is unsaved.
fn hand_off(
    editor: &mut Editor,
    request: Option<ReviewRequest>,
    target: Option<String>,
    command: &str,
) -> anyhow::Result<PathBuf> {
    let dir = editor
        .review_handoff_dir()
        .context("no review handoff directory, use :review-handoff <dir> first")?;
    ensure_reviews_saved(editor, command)?;
    if let Some(request) = request {
        editor.review_batch.set_request(request);
    }
    if let Some(target) = target {
        editor.review_batch.set_target(target);
    }
    export_handoff(editor, &dir)
}

/// Starts reviewing the written document against the review baseline, and exports the review
/// batch when `:review-comment` wrote it to send its comments.
pub(crate) fn document_written(editor: &mut Editor, doc_id: DocumentId) {
    if let Err(err) = editor.start_baseline_review(doc_id) {
        log::info!("not reviewing document {doc_id}: {err:#}");
    }
    if editor.review_send_on_write != Some(doc_id) {
        return;
    }
    editor.review_send_on_write = None;
    let Some(dir) = editor.review_handoff_dir() else {
        return;
    };
    let comments = editor.review_batch.comments().len();
    match export_handoff(editor, &dir) {
        Ok(_) if comments > 0 => {
            editor.set_status(format!("Sent {} to the agent", plural(comments, "comment")))
        }
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
    let target = editor.review_batch.target().map(str::to_owned);

    let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let path = dir.join(format!("{millis}-{}.json", std::process::id()));
    let temp = path.with_extension("json.tmp");
    editor
        .review_batch
        .export(&temp, |file| !modified.contains(file))?;
    std::fs::rename(&temp, &path)
        .with_context(|| format!("failed to rename {} to {}", temp.display(), path.display()))?;
    editor.review_target = target;
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
    let agents = agent_labels(&workspace);
    let files: Vec<_> = changes
        .into_iter()
        .map(|change| {
            let stats = file_stats(cx.editor, change.path(), &commit, trust_full);
            let agent = agents.get(change.path()).cloned();
            (change, stats, agent)
        })
        .collect();
    let counted = files.iter().filter_map(|(_, stats, _)| *stats);
    let (added, removed, staged, committed, total) = counted.fold((0, 0, 0, 0, 0), |sum, stats| {
        (
            sum.0 + stats.added,
            sum.1 + stats.removed,
            sum.2 + stats.staged,
            sum.3 + stats.committed,
            sum.4 + stats.hunks,
        )
    });
    let committed = match committed {
        0 => String::new(),
        n => format!(", {n} committed"),
    };
    let status = format!(
        "{}, +{added} -{removed}, {staged}/{total} hunks staged{committed}",
        plural(files.len(), "file")
    );
    cx.jobs.callback(async move {
        let callback: job::Callback =
            Callback::EditorCompositor(Box::new(move |editor, compositor| {
                type Item = (FileChange, Option<FileStats>, Option<String>);
                let columns = [
                    ui::PickerColumn::new("change", |(change, ..): &Item, _| {
                        match change {
                            FileChange::Untracked { .. } => "A",
                            FileChange::Deleted { .. } => "D",
                            _ => "M",
                        }
                        .into()
                    }),
                    ui::PickerColumn::new("path", |(change, ..): &Item, workspace: &PathBuf| {
                        let path = change.path();
                        path.strip_prefix(workspace)
                            .unwrap_or(path)
                            .display()
                            .to_string()
                            .into()
                    }),
                    ui::PickerColumn::new("+/-", |(_, stats, _): &Item, _| {
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
                    ui::PickerColumn::new("hunks", |(_, stats, _): &Item, _| {
                        match stats {
                            Some(stats) if stats.committed > 0 => format!(
                                "{}/{} {} committed",
                                stats.staged, stats.hunks, stats.committed
                            ),
                            Some(stats) => format!("{}/{}", stats.staged, stats.hunks),
                            None => "?".to_owned(),
                        }
                        .into()
                    }),
                    ui::PickerColumn::new("agent", |(.., agent): &Item, _| {
                        agent.as_deref().unwrap_or_default().into()
                    }),
                ];
                let picker = ui::Picker::new(
                    columns,
                    1,
                    files,
                    workspace,
                    |cx, (change, ..): &Item, action| {
                        cx.editor.nav_mode = helix_view::editor::NavMode::Review;
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
                .with_preview(|editor, (change, ..): &Item| {
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
    stage_and_move_on(cx, Staging::Stage)
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
    stage_and_move_on(cx, Staging::Unstage)
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
    stage_and_move_on(cx, Staging::Toggle)
}

/// Stages or unstages like [`change_staging`], then, after staging in a reviewed buffer, moves the
/// cursor on to the next unstaged hunk after the selections, in this file or the next changed one
/// (wrapping around); with none left anywhere, or after unstaging, the cursor stays.
fn stage_and_move_on(cx: &mut compositor::Context, staging: Staging) -> anyhow::Result<()> {
    cx.editor.nav_mode = helix_view::editor::NavMode::Review;
    let (view, doc) = current_ref!(cx.editor);
    let past = doc
        .selection(view.id)
        .iter()
        .map(|range| range.line_range(doc.text().slice(..)).1 + 1)
        .max()
        .unwrap_or(0);
    // The diff buffer has its own cursor to keep, and focused old text marks one block.
    let move_on = doc.review.is_some() && cx.editor.review_diff != Some(doc.id());
    let staged = change_staging(cx.editor, staging)?;
    if !move_on || !staged {
        return Ok(());
    }
    let status = cx.editor.get_status().map(|(status, _)| status.to_string());
    let from = doc!(cx.editor).path().map(Path::to_path_buf);
    match next_unstaged_hunk(cx, past) {
        Ok(true) => {
            let to = doc!(cx.editor).path().map(Path::to_path_buf);
            let target = cx.editor.get_status().map(|(status, _)| status.to_string());
            match (status, target) {
                (Some(status), Some(target)) if to != from => cx
                    .editor
                    .set_status(format!("{status}; next: {}", target.replacen(": ", " ", 1))),
                (Some(status), _) => cx.editor.set_status(status),
                _ => (),
            }
        }
        Ok(false) => {
            if let Some(status) = status {
                cx.editor.set_status(status);
            }
        }
        Err(err) => log::info!("not moving on after staging: {err:#}"),
    }
    Ok(())
}

/// Lands on the first unstaged hunk of the current buffer starting at or after line `past`, else
/// on the first unstaged hunk of the next changed file with any, wrapping around to the start of
/// the current buffer. Returns whether there was one.
fn next_unstaged_hunk(cx: &mut compositor::Context, past: usize) -> anyhow::Result<bool> {
    let unstaged = |doc: &Document| -> Vec<usize> {
        doc.review
            .as_ref()
            .map(Review::unstaged_hunk_lines)
            .unwrap_or_default()
    };
    let here: Vec<usize> = unstaged(doc!(cx.editor));
    if let Some(&line) = here.iter().filter(|&&line| line >= past).min() {
        land_on_hunk(cx.editor, line);
        announce_hunk(cx.editor);
        return Ok(true);
    }
    let (_, changes) = changed_files(cx)?;
    let trust_full = doc_trust_full(cx.editor);
    let files: Vec<PathBuf> = changes
        .iter()
        .filter(|change| matches!(change, FileChange::Modified { .. }))
        .map(|change| change.path().to_path_buf())
        .collect();
    let current = doc!(cx.editor).path().map(Path::to_path_buf);
    let start = current
        .as_ref()
        .and_then(|path| files.iter().position(|file| file == path));
    let order = match start {
        Some(idx) => files[idx + 1..]
            .iter()
            .chain(&files[..=idx])
            .collect::<Vec<_>>(),
        None => files.iter().collect(),
    };
    for file in order {
        if Some(file) == current.as_ref() {
            if let Some(&line) = here.iter().min() {
                land_on_hunk(cx.editor, line);
                announce_hunk(cx.editor);
                return Ok(true);
            }
            continue;
        }
        // Only a file whose worktree differs from the index has anything left to stage.
        let pending = cx
            .editor
            .diff_providers
            .get_index_base(file, trust_full)
            .ok()
            .zip(std::fs::read(file).ok())
            .is_some_and(|((index, _), worktree)| {
                !helix_vcs::line_hunks(&index, &worktree).is_empty()
            });
        if !pending {
            continue;
        }
        cx.editor.open(file, Action::Replace)?;
        if let Err(err) = ensure_review(cx) {
            log::info!("not enabling review: {err:#}");
        }
        if let Some(&line) = unstaged(doc!(cx.editor)).iter().min() {
            land_on_hunk(cx.editor, line);
            announce_hunk(cx.editor);
            return Ok(true);
        }
    }
    Ok(false)
}

/// Shows `<file>: hunk 2 of 5` for the hunk the cursor is on.
fn announce_hunk(editor: &mut Editor) {
    let (view, doc) = current_ref!(editor);
    let cursor = doc
        .selection(view.id)
        .primary()
        .cursor_line(doc.text().slice(..));
    let hunks = doc
        .review
        .as_ref()
        .map(Review::uncommitted_hunk_lines)
        .unwrap_or_default();
    let name = doc
        .path()
        .map(|path| {
            path.strip_prefix(helix_loader::find_workspace().0)
                .unwrap_or(path)
                .display()
                .to_string()
        })
        .unwrap_or_else(|| doc.display_name().into_owned());
    if let Some(idx) = hunks.iter().position(|lines| lines.start == cursor) {
        editor.set_status(format!("{name}: hunk {} of {}", idx + 1, hunks.len()));
    }
}

const NOTHING_TO_STAGE: &str = "nothing to stage here";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Staging {
    Stage,
    Unstage,
    Toggle,
}

/// Stages or unstages the hunk under the cursor, see `:review-stage` and `:review-unstage`.
/// Returns whether it staged.
fn change_staging(editor: &mut Editor, staging: Staging) -> anyhow::Result<bool> {
    let (path, targets) = stage_target(editor)?;
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
    let head = editor
        .diff_providers
        .get_commit_base(&path, "HEAD", trust_full)
        .ok();
    // The unstaged hunks (index to worktree) the selections meet, and the staged ones (baseline to
    // index) they meet outside every unstaged hunk, which map onto the index unchanged but for the
    // line count differences of the unstaged hunks before them.
    let unstaged = helix_vcs::line_hunks(&index, &worktree);
    let to_stage = hunks_meeting(&unstaged, &targets);
    let index_targets: Vec<_> = targets
        .iter()
        .map(|target| worktree_to_index_lines(&unstaged, target))
        .collect();
    let staged = helix_vcs::line_hunks(&base, &index);
    // A staged hunk HEAD already holds was committed after the baseline: there is nothing to toggle.
    let in_head = |hunk: &helix_vcs::Hunk| {
        head.as_ref().is_some_and(|head| {
            let lines = hunk.after.start as usize..hunk.after.end as usize;
            helix_vcs::find_hunk(&helix_vcs::line_hunks(&head.data, &index), &lines).is_none()
        })
    };
    let (committed, to_unstage): (Vec<_>, Vec<_>) = hunks_meeting(&staged, &index_targets)
        .into_iter()
        .partition(|hunk| in_head(hunk));
    if to_stage.is_empty() && to_unstage.is_empty() {
        if let (Some(head), false) = (&head, committed.is_empty()) {
            bail!("already committed in {}", &head.commit_id[..8]);
        }
    }

    let stage = match staging {
        Staging::Stage => true,
        Staging::Unstage => false,
        // Unstage only when every hunk met is already staged.
        Staging::Toggle => !to_stage.is_empty(),
    };
    let (content, verb, hunks) = if stage {
        ensure!(!to_stage.is_empty(), NOTHING_TO_STAGE);
        // Applied from the last, so the earlier hunks' index lines stay valid.
        let content = to_stage.iter().rev().fold(index.clone(), |content, hunk| {
            helix_vcs::apply_hunk(&content, &worktree, hunk)
        });
        (content, "Staged", to_stage.len())
    } else {
        ensure!(
            !to_unstage.is_empty(),
            match staging {
                Staging::Unstage => "nothing staged here",
                _ => NOTHING_TO_STAGE,
            }
        );
        let content = to_unstage
            .iter()
            .rev()
            .fold(index.clone(), |content, hunk| {
                let revert = helix_vcs::Hunk {
                    before: hunk.after.clone(),
                    after: hunk.before.clone(),
                };
                helix_vcs::apply_hunk(&content, &base, &revert)
            });
        (content, "Unstaged", to_unstage.len())
    };
    editor
        .diff_providers
        .update_index_entry(&path, &content, &expected, trust_full)?;

    let (count, total) = helix_vcs::staged_hunk_count(&base, &content, &worktree);
    if let Some(doc) = editor.document_by_path_mut(&path) {
        let text = doc.text().clone();
        if let Some(review) = doc.review.as_mut() {
            review.set_index(&content, head.as_ref().map(|head| &head.data[..]), &text);
        }
    }
    refresh_review_diff(editor);
    let name = path
        .strip_prefix(helix_loader::find_workspace().0)
        .unwrap_or(&path)
        .display()
        .to_string();
    let skipped = match committed.len() {
        0 => String::new(),
        n => format!(", {n} already committed"),
    };
    editor.set_status(format!(
        "{verb} {}{skipped}; {count} of {total} staged in {name}",
        plural(hunks, "hunk")
    ));
    Ok(stage)
}

/// The hunks of `hunks` (in order) that any of the `targets` line ranges meet. A single line meets
/// the one hunk [`helix_vcs::find_hunk`] finds for it; a wider range meets every hunk it touches.
fn hunks_meeting<'a>(
    hunks: &'a [helix_vcs::Hunk],
    targets: &[ops::Range<usize>],
) -> Vec<&'a helix_vcs::Hunk> {
    let mut met = vec![false; hunks.len()];
    for target in targets {
        if target.len() <= 1 {
            if let Some(hunk) = helix_vcs::find_hunk(hunks, target) {
                met[hunks.iter().position(|h| std::ptr::eq(h, hunk)).unwrap()] = true;
            }
        } else {
            for (idx, hunk) in hunks.iter().enumerate() {
                met[idx] |= helix_vcs::find_hunk(std::slice::from_ref(hunk), target).is_some();
            }
        }
    }
    hunks
        .iter()
        .zip(met)
        .filter(|(_, met)| *met)
        .map(|(hunk, _)| hunk)
        .collect()
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

/// The label of the pi session that last changed each file, from the hook's per-session state
/// files in `<workspace>/.pi/helix-review/state/`: a session covers the files in its turn
/// snapshots, and the most recently active session wins.
fn agent_labels(workspace: &Path) -> HashMap<PathBuf, String> {
    #[derive(serde::Deserialize)]
    struct State {
        label: Option<String>,
        #[serde(default)]
        snapshots: HashMap<PathBuf, String>,
        #[serde(default)]
        last_active: u64,
    }
    let dir = workspace.join(".pi/helix-review/state");
    let mut states: Vec<State> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok())
        .collect();
    states.sort_by_key(|state| state.last_active);
    let mut labels = HashMap::new();
    for state in states {
        if let Some(label) = state.label {
            for path in state.snapshots.into_keys() {
                labels.insert(path, label.clone());
            }
        }
    }
    labels
}

/// Line and hunk counts of a file's change since the review baseline, for `:review-files`.
#[derive(Clone, Copy)]
struct FileStats {
    added: usize,
    removed: usize,
    /// Hunks the git index already holds in full, but `HEAD` does not.
    staged: usize,
    /// Hunks `HEAD` already holds: committed after the review baseline.
    committed: usize,
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
    let committed = match editor
        .diff_providers
        .get_commit_base(path, "HEAD", trust_full)
    {
        Ok(head) => helix_vcs::staged_hunk_count(&before, &head.data, &after).0,
        Err(_) => 0,
    };
    Some(FileStats {
        added: hunks.iter().map(|hunk| hunk.after.len()).sum(),
        removed: hunks.iter().map(|hunk| hunk.before.len()).sum(),
        // Counts, not per-hunk sets: a committed hunk that the index reverts is miscounted.
        staged: staged.saturating_sub(committed),
        committed,
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
    let mut cx = compositor::Context {
        editor: cx.editor,
        jobs: cx.jobs,
        scroll: None,
    };
    if let Err(err) = toggle_or_jump(&mut cx) {
        cx.editor.set_error(format!("{err:#}"));
    }
}

/// The `G s` key: toggles the staging like `:review-toggle-stage`, or jumps on like Enter.
pub(crate) fn review_toggle_stage_key(cx: &mut Context) {
    let mut cx = compositor::Context {
        editor: cx.editor,
        jobs: cx.jobs,
        scroll: None,
    };
    let result = ensure_review(&mut cx).and_then(|()| toggle_or_jump(&mut cx));
    if let Err(err) = result {
        cx.editor.set_error(format!("{err:#}"));
    }
}

/// Toggles the staging of the hunks under the selections; when there is none to stage in a
/// reviewed buffer, jumps to the next unstaged hunk instead, across files.
fn toggle_or_jump(cx: &mut compositor::Context) -> anyhow::Result<()> {
    let err = match stage_and_move_on(cx, Staging::Toggle) {
        Err(err) if err.to_string() == NOTHING_TO_STAGE => err,
        result => return result,
    };
    let doc = doc!(cx.editor);
    if doc.review.is_none() || cx.editor.review_diff == Some(doc.id()) {
        return Err(err);
    }
    let (view, doc) = current_ref!(cx.editor);
    let cursor = doc
        .selection(view.id)
        .primary()
        .cursor_line(doc.text().slice(..));
    if !next_unstaged_hunk(cx, cursor)? {
        let commit = cx.editor.review_baseline.clone().unwrap_or_default();
        bail!("nothing unstaged since {}", &commit[..8.min(commit.len())]);
    }
    if let Some((status, _)) = cx.editor.get_status() {
        let status = format!("next: {}", status.replacen(": ", " ", 1));
        cx.editor.set_status(status);
    }
    Ok(())
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

/// The file and worktree line ranges `:review-stage` stages the hunks of: the hunk under the cursor
/// of the `:review-diff` buffer, the focused old text's block, or the lines of every selection in
/// a reviewed buffer.
fn stage_target(editor: &Editor) -> anyhow::Result<(PathBuf, Vec<ops::Range<usize>>)> {
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
        return Ok((path, std::iter::once(line - 1..line).collect()));
    }
    let review = doc
        .review
        .as_ref()
        .context("not reviewing this buffer, use :review first")?;
    ensure!(!review.is_suspended(), Review::LIMIT_MESSAGE);
    let path = doc.path().context("buffer has no path")?.to_path_buf();
    let lines = match review.focused_block_index(view.id) {
        Some(idx) => vec![review.blocks()[idx].current.clone()],
        // A displayed block acts as one unit, even where its lines span several staging hunks
        // (part committed after the baseline, part edited since): each selection grows to the
        // blocks it meets.
        None => doc
            .selection(view.id)
            .iter()
            .map(|range| {
                let (start, end) = range.line_range(text);
                review
                    .changed_lines()
                    .filter(|block| block.start <= end && start < block.end)
                    .fold(start..end + 1, |lines, block| {
                        lines.start.min(block.start)..lines.end.max(block.end)
                    })
            })
            .collect(),
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
    // Review menu entries that run typable commands allowed on old text.
    if OLD_FOCUS_STATIC.contains(&name) {
        if let Ok(command) = name.parse::<MappableCommand>() {
            command.execute(cx);
        }
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
