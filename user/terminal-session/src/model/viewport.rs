//! Bounded scrollback viewport over the primary VT history.

use super::{Cell, Model};

impl Model {
    /// Moves the viewport relative to the live bottom.
    ///
    /// Positive `lines` browse older history and negative values move toward
    /// the live screen. The offset is clamped to retained primary-screen history.
    ///
    /// Returns `true` when the projected viewport changed.
    pub(crate) fn scroll_viewport(&mut self, lines: i32) -> bool {
        if self.alternate_active {
            return false;
        }
        let next = if lines >= 0 {
            self.viewport_offset
                .saturating_add(lines as usize)
                .min(self.history.len())
        } else {
            self.viewport_offset
                .saturating_sub(lines.unsigned_abs() as usize)
        };
        if next == self.viewport_offset {
            return false;
        }
        self.viewport_offset = next;
        self.clear_selection();
        self.mark_all();
        true
    }

    /// Returns the number of retained lines above the live bottom.
    pub(crate) fn viewport_offset(&self) -> usize {
        self.viewport_offset
    }

    /// Returns the number of retained primary-screen history rows.
    pub(crate) fn history_rows(&self) -> usize {
        if self.alternate_active {
            0
        } else {
            self.history.len()
        }
    }

    /// Returns one cell from the currently projected viewport.
    pub(crate) fn projected_cell(&self, row: usize, column: usize) -> Cell {
        if self.alternate_active || self.viewport_offset == 0 {
            return self.cell_at(row, column);
        }
        let source_row = self.history.len() - self.viewport_offset + row;
        if source_row < self.history.len() {
            return self.history.row(source_row)[column];
        }
        self.cell_at(source_row - self.history.len(), column)
    }

    pub(super) fn preserve_viewport_on_history_push(&mut self, count: usize) {
        if self.viewport_offset != 0 {
            self.viewport_offset = self
                .viewport_offset
                .saturating_add(count)
                .min(self.history.len());
        }
    }

    pub(super) fn cell_at(&self, row: usize, column: usize) -> Cell {
        unsafe { *self.active().cells.add(row * self.columns + column) }
    }
}
