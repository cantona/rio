// grid/resize.rs was originally taken from Alacritty
// https://github.com/alacritty/alacritty/blob/e35e5ad14fce8456afdd89f2b392b9924bb27471/alacritty_terminal/src/grid/resize.rs
// which is licensed under Apache 2.0 license.

use crate::crosswords::grid::{Dimensions, Grid};
use crate::crosswords::pos::{Boundary, Column, Line};
use crate::crosswords::square::{Square, Wide};
use crate::crosswords::Row;
use std::cmp::{max, min, Ordering};
use std::mem;

/// True when the row anchors an inline image (sixel/iTerm2). Such rows
/// must not reflow on a column change: every covered cell of one image
/// row shares a single extras slot whose x offsets derive from the
/// anchor column, so wrapping the cells onto another row paints the
/// same band twice at the wrong position. The row is clipped to the
/// new width instead, like a non-reflowing terminal.
fn row_has_graphics(row: &Row<Square>) -> bool {
    if !row.has_extras {
        return false;
    }
    (0..row.len()).any(|i| {
        let cell = &row[Column(i)];
        cell.has_graphics() && !cell.is_bg_only()
    })
}

impl Grid<Square> {
    /// Re-cover an image row's freshly grown columns. A column shrink
    /// clips an image row's covered cells, but the pixels live in the
    /// texture and every covered cell of one image row shares a single
    /// extras slot — so the clipped cells are fully reconstructible
    /// from the slot's anchor and texture width. Growing back restores
    /// the picture instead of leaving it cropped.
    fn recover_graphic_cells(&mut self, row: &mut Row<Square>, from: usize, to: usize) {
        use crate::crosswords::square::CellFlags;
        if !row.has_extras || from == 0 {
            return;
        }
        let mut spans: Vec<(crate::crosswords::square::ExtrasId, usize)> = Vec::new();
        for i in 0..from {
            let cell = &row[Column(i)];
            if !cell.has_graphics() || cell.is_bg_only() {
                continue;
            }
            let Some(eid) = cell.extras_id() else {
                continue;
            };
            if spans.iter().any(|(e, _)| *e == eid) {
                continue;
            }
            let Some(g) = self
                .extras_table
                .get(eid)
                .and_then(|e| e.graphic.as_ref())
                .and_then(|g| g.first())
            else {
                continue;
            };
            let cell_w = g.texture.cell_width.max(1);
            let span_end =
                g.anchor_col as usize + (g.texture.width as usize).div_ceil(cell_w);
            if span_end > from {
                spans.push((eid, span_end));
            }
        }
        for (eid, span_end) in spans {
            for col in from..to.min(span_end) {
                let cell = &mut row[Column(col)];
                cell.set_extras_id(Some(eid));
                cell.insert_cell_flag(CellFlags::GRAPHICS);
            }
        }
    }

    /// Resize the grid's width and/or height.
    pub fn resize(&mut self, reflow: bool, lines: usize, columns: usize) {
        // Use empty template cell for resetting cells due to resize.
        let template = mem::take(&mut self.cursor.template);

        match self.lines.cmp(&lines) {
            Ordering::Less => self.grow_lines(lines),
            Ordering::Greater => self.shrink_lines(lines),
            Ordering::Equal => (),
        }

        match self.columns.cmp(&columns) {
            Ordering::Less => self.grow_columns(reflow, columns),
            Ordering::Greater => self.shrink_columns(reflow, columns),
            Ordering::Equal => (),
        }

        // Restore template cell.
        self.cursor.template = template;
    }

    /// Add lines to the visible area.
    ///
    /// Rio keeps the cursor at the bottom of the terminal as long as there
    /// is scrollback available. Once scrollback is exhausted, new lines are
    /// simply added to the bottom of the screen.
    fn grow_lines(&mut self, target: usize) {
        let lines_added = target - self.lines;

        // Need to resize before updating buffer.
        self.raw.grow_visible_lines(target);
        self.lines = target;

        let history_size = self.history_size();
        let from_history = min(history_size, lines_added);

        // Move existing lines up for every line that couldn't be pulled from history.
        if from_history != lines_added {
            let delta = lines_added - from_history;
            self.scroll_up(&(Line(0)..Line(target as i32)), delta);
        }

        // Move cursor down for every line pulled from history.
        self.saved_cursor.pos.row += from_history;
        self.cursor.pos.row += from_history;

        self.display_offset = self.display_offset.saturating_sub(lines_added);
        self.decrease_scroll_limit(lines_added);
    }

    /// Remove lines from the visible area.
    ///
    /// The behavior in Terminal.app and iTerm.app is to keep the cursor at the
    /// bottom of the screen. This is achieved by pushing history "out the top"
    /// of the terminal window.
    ///
    /// Rio takes the same approach.
    fn shrink_lines(&mut self, target: usize) {
        // Scroll up to keep content inside the window.
        let required_scrolling =
            (self.cursor.pos.row.0 as usize + 1).saturating_sub(target);
        if required_scrolling > 0 {
            self.scroll_up(&(Line(0)..Line(self.lines as i32)), required_scrolling);

            // Clamp cursors to the new viewport size.
            self.cursor.pos.row = min(self.cursor.pos.row, Line(target as i32 - 1));
        }

        // Clamp saved cursor, since only primary cursor is scrolled into viewport.
        self.saved_cursor.pos.row =
            min(self.saved_cursor.pos.row, Line(target as i32 - 1));

        self.raw.rotate((self.lines - target) as isize);
        self.raw.shrink_visible_lines(target);
        self.lines = target;
    }

    /// Grow number of columns in each row, reflowing if necessary.
    fn grow_columns(&mut self, reflow: bool, columns: usize) {
        // Check if a row needs to be wrapped.
        let should_reflow = |row: &Row<Square>| -> bool {
            let len = Column(row.len());
            reflow && len.0 > 0 && len < columns && row[len - 1].wrapline()
        };

        self.columns = columns;

        let mut reversed: Vec<Row<Square>> = Vec::with_capacity(self.raw.len());
        let mut cursor_line_delta = 0;

        // Remove the linewrap special case, by moving the cursor outside of the grid.
        if self.cursor.should_wrap && reflow {
            self.cursor.should_wrap = false;
            self.cursor.pos.col += 1;
        }

        let mut rows = self.raw.take_all();

        for (i, mut row) in rows.drain(..).enumerate().rev() {
            // Check if reflowing should be performed. An image row is a
            // barrier: cells are neither pulled from it nor into it.
            let last_row = match reversed.last_mut() {
                Some(last_row)
                    if should_reflow(last_row)
                        && !row_has_graphics(&row)
                        && !row_has_graphics(last_row) =>
                {
                    last_row
                }
                _ => {
                    reversed.push(row);
                    continue;
                }
            };

            // Remove wrap flag before appending additional cells.
            if let Some(cell) = last_row.last_mut() {
                cell.set_wrapline(false);
            }

            // Remove leading spacers when reflowing wide char to the previous line.
            let mut last_len = last_row.len();
            if last_len >= 1
                && matches!(last_row[Column(last_len - 1)].wide(), Wide::LeadingSpacer)
            {
                last_row.shrink(last_len - 1);
                last_len -= 1;
            }

            // Don't try to pull more cells from the next line than available.
            let mut num_wrapped = columns - last_len;
            let len = min(row.len(), num_wrapped);

            // Insert leading spacer when there's not enough room for reflowing wide char.
            let mut cells = if matches!(row[Column(len - 1)].wide(), Wide::Wide) {
                num_wrapped -= 1;

                let mut cells = row.front_split_off(len - 1);

                let mut spacer = Square::default();
                spacer.set_wide(Wide::LeadingSpacer);
                cells.push(spacer);

                cells
            } else {
                row.front_split_off(len)
            };

            // Add removed cells to previous row and reflow content.
            last_row.append(&mut cells);

            let cursor_buffer_line = self.lines - self.cursor.pos.row.0 as usize - 1;

            if i == cursor_buffer_line && reflow {
                // Resize cursor's line and reflow the cursor if necessary.
                let mut target = self.cursor.pos.sub(self, Boundary::Cursor, num_wrapped);

                // Clamp to the last column, if no content was reflown with the cursor.
                if target.col.0 == 0 && row.is_clear() {
                    self.cursor.should_wrap = true;
                    target = target.sub(self, Boundary::Cursor, 1);
                }
                self.cursor.pos.col = target.col;

                // Get required cursor line changes. Since `num_wrapped` is smaller than `columns`
                // this will always be either `0` or `1`.
                let line_delta = self.cursor.pos.row - target.row;

                if line_delta != 0 && row.is_clear() {
                    continue;
                }

                cursor_line_delta += line_delta.0 as usize;
            } else if row.is_clear() {
                if i < self.display_offset {
                    // Since we removed a line, rotate down the viewport.
                    self.display_offset = self.display_offset.saturating_sub(1);
                }

                // Rotate cursor down if content below them was pulled from history.
                if i < cursor_buffer_line {
                    self.cursor.pos.row += 1;
                }

                // Don't push line into the new buffer.
                continue;
            }

            if let Some(cell) = last_row.last_mut() {
                // Set wrap flag if next line still has cells.
                cell.set_wrapline(true);
            }

            reversed.push(row);
        }

        // Make sure we have at least the viewport filled.
        if reversed.len() < self.lines {
            let delta = (self.lines - reversed.len()) as i32;
            self.cursor.pos.row = max(self.cursor.pos.row - delta, Line(0));
            reversed.resize_with(self.lines, || Row::new(columns));
        }

        // Pull content down to put cursor in correct position, or move cursor up if there's no
        // more lines to delete below the cursor.
        if cursor_line_delta != 0 {
            let cursor_buffer_line = self.lines - self.cursor.pos.row.0 as usize - 1;
            let available = min(cursor_buffer_line, reversed.len() - self.lines);
            let overflow = cursor_line_delta.saturating_sub(available);
            reversed.truncate(reversed.len() + overflow - cursor_line_delta);
            self.cursor.pos.row = max(self.cursor.pos.row - overflow, Line(0));
        }

        // Reverse iterator and fill all rows that are still too short.
        let mut new_raw = Vec::with_capacity(reversed.len());
        for mut row in reversed.drain(..).rev() {
            if row.len() < columns {
                let covered = row.len();
                row.grow(columns);
                self.recover_graphic_cells(&mut row, covered, columns);
            }
            new_raw.push(row);
        }

        self.raw.replace_inner(new_raw);

        // Clamp display offset in case lines above it got merged.
        self.display_offset = min(self.display_offset, self.history_size());
    }

    /// Shrink number of columns in each row, reflowing if necessary.
    fn shrink_columns(&mut self, reflow: bool, columns: usize) {
        self.columns = columns;

        // Remove the linewrap special case, by moving the cursor outside of the grid.
        if self.cursor.should_wrap && reflow {
            self.cursor.should_wrap = false;
            self.cursor.pos.col += 1;
        }

        let mut new_raw = Vec::with_capacity(self.raw.len());
        let mut buffered: Option<Vec<Square>> = None;

        let mut rows = self.raw.take_all();
        for (i, mut row) in rows.drain(..).enumerate().rev() {
            // Append lines left over from the previous row.
            if let Some(buffered) = buffered.take() {
                if row_has_graphics(&row) {
                    // Wrapped text must not flow into an image row —
                    // prepending would shift the image cells off their
                    // anchor column. Give the leftover cells their own
                    // line above instead.
                    let occ = buffered.len();
                    let mut spill = Row::from_vec(buffered, occ);
                    if spill.len() < columns {
                        spill.grow(columns);
                    }
                    new_raw.push(spill);
                    if i < self.display_offset {
                        self.display_offset += 1;
                    }
                } else {
                    // Add a column for every cell added before the cursor, if it goes beyond the new
                    // width it is then later reflown.
                    let cursor_buffer_line =
                        self.lines - self.cursor.pos.row.0 as usize - 1;
                    if i == cursor_buffer_line {
                        self.cursor.pos.col += buffered.len();
                    }

                    row.append_front(buffered);
                }
            }

            // An image row never reflows: clip it to the new width and
            // keep it whole. The clipped cells' extras slots are
            // reclaimed by the next gc_extras sweep.
            if row_has_graphics(&row) {
                let _ = row.shrink(columns);
                new_raw.push(row);
                continue;
            }

            loop {
                // Remove all cells which require reflowing.
                let mut wrapped = match row.shrink(columns) {
                    Some(wrapped) if reflow => wrapped,
                    _ => {
                        let cursor_buffer_line =
                            self.lines - self.cursor.pos.row.0 as usize - 1;
                        if reflow
                            && i == cursor_buffer_line
                            && self.cursor.pos.col > columns
                        {
                            // If there are empty cells before the cursor, we assume it is explicit
                            // whitespace and need to wrap it like normal content.
                            Vec::new()
                        } else {
                            // Since it fits, just push the existing line without any reflow.
                            new_raw.push(row);
                            break;
                        }
                    }
                };

                // Insert spacer if a wide char would be wrapped into the last column.
                if row.len() >= columns
                    && matches!(row[Column(columns - 1)].wide(), Wide::Wide)
                {
                    let mut spacer = Square::default();
                    spacer.set_wide(Wide::LeadingSpacer);

                    let wide_char = mem::replace(&mut row[Column(columns - 1)], spacer);
                    wrapped.insert(0, wide_char);
                }

                // Remove wide char spacer before shrinking.
                let len = wrapped.len();
                if len > 0 && matches!(wrapped[len - 1].wide(), Wide::LeadingSpacer) {
                    if len == 1 {
                        row[Column(columns - 1)].set_wrapline(true);
                        new_raw.push(row);
                        break;
                    } else {
                        // Remove the leading spacer from the end of the wrapped row.
                        wrapped[len - 2].set_wrapline(true);
                        wrapped.truncate(len - 1);
                    }
                }

                new_raw.push(row);

                // Set line as wrapped if cells got removed.
                if let Some(cell) = new_raw.last_mut().and_then(|r| r.last_mut()) {
                    cell.set_wrapline(true);
                }

                if wrapped
                    .last()
                    .map(|c| c.wrapline() && i >= 1)
                    .unwrap_or(false)
                    && wrapped.len() < columns
                {
                    // Make sure previous wrap flag doesn't linger around.
                    if let Some(cell) = wrapped.last_mut() {
                        cell.set_wrapline(false);
                    }

                    // Add removed cells to start of next row.
                    buffered = Some(wrapped);
                    break;
                } else {
                    // Reflow cursor if a line below it is deleted.
                    let cursor_buffer_line =
                        self.lines - self.cursor.pos.row.0 as usize - 1;
                    if (i == cursor_buffer_line && self.cursor.pos.col < columns)
                        || i < cursor_buffer_line
                    {
                        self.cursor.pos.row = max(self.cursor.pos.row - 1, Line(0));
                    }

                    // Reflow the cursor if it is on this line beyond the width.
                    if i == cursor_buffer_line && self.cursor.pos.col >= columns {
                        // Since only a single new line is created, we subtract only `columns`
                        // from the cursor instead of reflowing it completely.
                        self.cursor.pos.col -= columns;
                    }

                    // Make sure new row is at least as long as new width.
                    let occ = wrapped.len();
                    if occ < columns {
                        wrapped.resize_with(columns, Square::default);
                    }
                    row = Row::from_vec(wrapped, occ);

                    if i < self.display_offset {
                        // Since we added a new line, rotate up the viewport.
                        self.display_offset += 1;
                    }
                }
            }
        }

        // Reverse iterator and use it as the new grid storage.
        let mut reversed: Vec<Row<Square>> = new_raw.drain(..).rev().collect();
        reversed.truncate(self.max_scroll_limit + self.lines);
        self.raw.replace_inner(reversed);

        // Clamp display offset in case some lines went off.
        self.display_offset = min(self.display_offset, self.history_size());

        // Reflow the primary cursor, or clamp it if reflow is disabled.
        if !reflow {
            self.cursor.pos.col = min(self.cursor.pos.col, Column(columns - 1));
        } else if self.cursor.pos.col == columns
            && !self[self.cursor.pos.row][Column(columns - 1)].wrapline()
        {
            self.cursor.should_wrap = true;
            self.cursor.pos.col -= 1;
        } else {
            self.cursor.pos = self.cursor.pos.grid_clamp(self, Boundary::Cursor);
        }

        // Clamp the saved cursor to the grid.
        self.saved_cursor.pos.col = min(self.saved_cursor.pos.col, Column(columns - 1));
    }
}
