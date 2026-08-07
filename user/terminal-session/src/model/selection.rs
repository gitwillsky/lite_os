//! Visible-grid terminal selection owned by the VT model.

use super::{ATTR_HIDDEN, Model, SOFT_WRAPPED_ROW, WIDE_CONTINUATION};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CellPosition {
    pub(crate) row: usize,
    pub(crate) column: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Selection {
    anchor: CellPosition,
    focus: CellPosition,
}

/// One normalized inclusive selection projected into the terminal update protocol.
pub(crate) struct SelectionProjection {
    pub(crate) start: CellPosition,
    pub(crate) end: CellPosition,
    pub(crate) text: String,
}

impl Selection {
    fn normalized(self) -> (CellPosition, CellPosition) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }
}

impl Model {
    /// Replaces the current visible-grid selection.
    ///
    /// `anchor_column`/`anchor_row` identify the pointer-down cell and
    /// `focus_column`/`focus_row` identify the latest drag cell. Coordinates
    /// are clamped to the current grid and wide-character continuation cells
    /// resolve to their leading cell.
    ///
    /// Returns `true` when the selection projection changed.
    pub(crate) fn set_selection(
        &mut self,
        anchor_column: usize,
        anchor_row: usize,
        focus_column: usize,
        focus_row: usize,
    ) -> bool {
        let next = Selection {
            anchor: self.selection_position(anchor_column, anchor_row),
            focus: self.selection_position(focus_column, focus_row),
        };
        if self.selection == Some(next) {
            return false;
        }
        self.selection = Some(next);
        self.selection_dirty = true;
        true
    }

    /// Clears the current selection and returns whether a projection changed.
    pub(crate) fn clear_selection(&mut self) -> bool {
        if self.selection.take().is_none() {
            return false;
        }
        self.selection_dirty = true;
        true
    }

    /// Returns whether selection state changed after the last published update.
    pub(crate) fn selection_dirty(&self) -> bool {
        self.selection_dirty
    }

    /// Marks the current selection projection as published.
    pub(crate) fn clear_selection_dirty(&mut self) {
        self.selection_dirty = false;
    }

    /// Builds the normalized visible selection and its exact UTF-8 text.
    pub(crate) fn selection_projection(&self) -> Option<SelectionProjection> {
        let (start, end) = self.selection?.normalized();
        let mut text = String::new();
        for row in start.row..=end.row {
            let first = if row == start.row { start.column } else { 0 };
            let last = if row == end.row {
                end.column
            } else {
                self.columns - 1
            };
            for column in first..=last {
                let cell = self.projected_cell(row, column);
                if cell.reserved & WIDE_CONTINUATION != 0 {
                    continue;
                }
                let character = if cell.attributes & ATTR_HIDDEN != 0 {
                    ' '
                } else {
                    char::from_u32(cell.codepoint).unwrap_or('\u{fffd}')
                };
                text.push(character);
            }
            if row != end.row && !self.row_soft_wrapped(row) {
                text.push('\n');
            }
        }
        Some(SelectionProjection { start, end, text })
    }

    fn selection_position(&self, column: usize, row: usize) -> CellPosition {
        let row = row.min(self.rows - 1);
        let mut column = column.min(self.columns - 1);
        if self.projected_cell(row, column).reserved & WIDE_CONTINUATION != 0 {
            column = column.saturating_sub(1);
        }
        CellPosition { row, column }
    }

    fn row_soft_wrapped(&self, row: usize) -> bool {
        self.projected_cell(row, self.columns - 1).reserved & SOFT_WRAPPED_ROW != 0
    }
}
