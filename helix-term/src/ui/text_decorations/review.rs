use helix_core::doc_formatter::{DocumentFormatter, FormattedGrapheme, TextFormat};
use helix_core::line_ending::line_end_char_index;
use helix_core::text_annotations::TextAnnotations;
use helix_core::{Position, RopeSlice};
use helix_view::graphics::{Modifier, Rect};
use helix_view::review::{HunkStage, OldFocus, Review};
use helix_view::theme::Style;
use helix_view::Theme;

use crate::ui::document::{LinePos, TextRenderer};
use crate::ui::text_decorations::Decoration;

/// Draws the old lines of a review into the virtual rows reserved below document lines by
/// `helix_view::review::ReviewLines`.
pub struct ReviewDecoration<'a> {
    review: &'a Review,
    text: RopeSlice<'a>,
    text_fmt: TextFormat,
    focus: Option<OldFocus>,
    /// Gutter column for the removed-line marker.
    marker_col: Option<u16>,
    old: Style,
    /// The old style for blocks whose hunk the git index already holds.
    old_staged: Style,
    /// The old style for blocks whose hunk was committed after the baseline.
    old_committed: Style,
    selection: Style,
    cursor: Style,
    /// The style of pull request comment threads.
    thread: Style,
    /// Document lines with old lines or threads below them, ascending.
    anchors: Vec<usize>,
    next_anchor: usize,
    /// Document line whose end was just rendered and that has old lines below it.
    pending_line: Option<usize>,
}

impl<'a> ReviewDecoration<'a> {
    pub fn new(
        review: &'a Review,
        text: RopeSlice<'a>,
        text_fmt: TextFormat,
        focus: Option<OldFocus>,
        marker_col: Option<u16>,
        theme: &Theme,
    ) -> Self {
        let mut anchors: Vec<usize> = (review.blocks().iter())
            .map(|block| block.anchor_line)
            .chain(review.thread_lines(text.len_lines()))
            .collect();
        anchors.sort_unstable();
        anchors.dedup();
        ReviewDecoration {
            review,
            text,
            text_fmt,
            focus,
            marker_col,
            old: theme.get("diff.minus.review"),
            old_staged: theme.get("diff.minus.review.staged"),
            old_committed: theme
                .try_get_exact("diff.minus.review.committed")
                .unwrap_or_else(|| theme.get("diff.minus.review.staged")),
            selection: theme.get("ui.selection.primary"),
            cursor: theme.get("ui.cursor.primary"),
            thread: theme.get("ui.review.thread"),
            anchors,
            next_anchor: 0,
            pending_line: None,
        }
    }

    fn anchor_char(&self, anchor: usize) -> usize {
        self.anchors
            .get(anchor)
            .map_or(usize::MAX, |&line| line_end_char_index(&self.text, line))
    }

    /// Draws snapshot line `line` in `old` style starting at `row` and returns the number of rows
    /// it took.
    fn draw_old_line(
        &self,
        renderer: &mut TextRenderer,
        row: usize,
        line: usize,
        old: Style,
    ) -> usize {
        let height = self.review.old_line_height(line, &self.text_fmt);
        let visible_rows =
            renderer.offset.row..renderer.offset.row + renderer.viewport.height as usize;
        if row + height <= visible_rows.start || row >= visible_rows.end {
            return height;
        }
        self.fill_rows(renderer, row, height, old);
        let snapshot = self.review.snapshot().slice(..);
        let line_start = snapshot.line_to_char(line);
        let has_line_ending =
            line_end_char_index(&snapshot, line) < snapshot.line_to_char(line + 1);
        if let Some(col) = self.marker_col.filter(|_| is_visible(renderer, row)) {
            renderer.set_string(col, row as u16, "-", old);
        }

        let annotations = TextAnnotations::default();
        let formatter = DocumentFormatter::new_at_prev_checkpoint(
            self.review.old_line(line),
            &self.text_fmt,
            &annotations,
            0,
        );
        for grapheme in formatter {
            let style =
                self.grapheme_style(&grapheme, line_start + grapheme.char_idx, snapshot, old);
            // The end of the line stands for its line ending, which is only drawn when focused.
            if grapheme.source.is_eof() && !(has_line_ending && style != old) {
                continue;
            }
            if !renderer.column_in_bounds(grapheme.visual_pos.col, grapheme.width()) {
                continue;
            }
            let col = grapheme.visual_pos.col - renderer.offset.col;
            renderer.draw_decoration_grapheme(
                grapheme.raw,
                style,
                (row + grapheme.visual_pos.row) as u16,
                col as u16,
            );
        }
        height
    }

    /// Paints the visible part of `height` virtual rows starting at `row` with `old`, so its
    /// background covers the whole row rather than only the drawn graphemes.
    fn fill_rows(&self, renderer: &mut TextRenderer, row: usize, height: usize, old: Style) {
        let visible = renderer.offset.row..renderer.offset.row + renderer.viewport.height as usize;
        let top = row.max(visible.start);
        let bottom = (row + height).min(visible.end);
        if top < bottom {
            let area = Rect::new(
                renderer.viewport.x,
                top as u16,
                renderer.viewport.width,
                (bottom - top) as u16,
            );
            renderer.set_style(area, old);
        }
    }

    fn grapheme_style(
        &self,
        grapheme: &FormattedGrapheme,
        char_idx: usize,
        snapshot: RopeSlice,
        old: Style,
    ) -> Style {
        let Some(focus) = self.focus.filter(|_| !grapheme.is_virtual()) else {
            return old;
        };
        if focus.range.cursor(snapshot) == char_idx {
            old.patch(self.cursor)
        } else if focus.range.from() <= char_idx && char_idx < focus.range.to() {
            old.patch(self.selection)
        } else {
            old
        }
    }
}

fn is_visible(renderer: &TextRenderer, row: usize) -> bool {
    (renderer.offset.row..renderer.offset.row + renderer.viewport.height as usize).contains(&row)
}

impl Decoration for ReviewDecoration<'_> {
    fn reset_pos(&mut self, pos: usize) -> usize {
        self.pending_line = None;
        self.next_anchor = self
            .anchors
            .partition_point(|&line| line_end_char_index(&self.text, line) < pos);
        self.anchor_char(self.next_anchor)
    }

    fn decorate_grapheme(
        &mut self,
        _renderer: &mut TextRenderer,
        _grapheme: &FormattedGrapheme,
    ) -> usize {
        self.pending_line = Some(self.anchors[self.next_anchor]);
        self.next_anchor += 1;
        self.anchor_char(self.next_anchor)
    }

    fn render_virt_lines(
        &mut self,
        renderer: &mut TextRenderer,
        pos: LinePos,
        virt_off: Position,
    ) -> Position {
        let Some(line) = self.pending_line.take() else {
            return Position::new(0, 0);
        };
        let first_row = pos.visual_line as usize + virt_off.row;
        let mut row = first_row;
        for block in self.review.blocks_at(line) {
            let old = match self.review.block_stage(block) {
                HunkStage::Unstaged => self.old,
                HunkStage::Staged => self.old_staged,
                HunkStage::Committed => self.old_committed,
            };
            if let Some(label) = block.label() {
                if is_visible(renderer, row) {
                    self.fill_rows(renderer, row, 1, old);
                    renderer.set_string_truncated(
                        renderer.viewport.x,
                        row as u16,
                        &label,
                        renderer.viewport.width as usize,
                        |_| old.add_modifier(Modifier::ITALIC),
                        true,
                        false,
                    );
                }
                row += 1;
            }
            for old_line in block.old.clone() {
                row += self.draw_old_line(renderer, row, old_line, old);
            }
        }
        let width = self.text_fmt.viewport_width as usize;
        for text in self
            .review
            .thread_rows_below(line, self.text.len_lines(), width)
        {
            if is_visible(renderer, row) {
                self.fill_rows(renderer, row, 1, self.thread);
                renderer.set_string_truncated(
                    renderer.viewport.x,
                    row as u16,
                    &text,
                    renderer.viewport.width as usize,
                    |_| self.thread,
                    true,
                    false,
                );
            }
            row += 1;
        }
        Position::new(row - first_row, 0)
    }
}
