use core::ptr;

use super::*;

// The primary screen retains a bounded 2048-row ring. Preallocating it keeps ordinary output and
// scroll allocation-free; without the bound an untrusted PTY child could grow terminal memory
// indefinitely, while omitting history makes resize reorder live output around stale visible rows.
const SCROLLBACK_ROWS: usize = 2048;

pub(super) struct History {
    cells: Vec<Cell>,
    start: usize,
    len: usize,
    columns: usize,
}

impl History {
    pub(super) fn new(columns: usize, blank: Cell) -> Option<Self> {
        let count = columns.checked_mul(SCROLLBACK_ROWS)?;
        let mut cells = Vec::new();
        cells.try_reserve_exact(count).ok()?;
        cells.resize(count, blank);
        Some(Self {
            cells,
            start: 0,
            len: 0,
            columns,
        })
    }

    pub(super) fn clear(&mut self) {
        self.start = 0;
        self.len = 0;
    }

    pub(super) fn push_screen_row(&mut self, screen: Screen, row: usize) {
        // SAFETY: callers pass a row inside the model's primary grid, and History is recreated with
        // that same column count on every resize. Reading exactly one row therefore stays inside
        // the live screen allocation.
        let cells = unsafe {
            core::slice::from_raw_parts(screen.cells.add(row * self.columns), self.columns)
        };
        self.push_row(cells);
    }

    fn push_row(&mut self, cells: &[Cell]) {
        debug_assert_eq!(cells.len(), self.columns);
        let slot = if self.len < SCROLLBACK_ROWS {
            let slot = (self.start + self.len) % SCROLLBACK_ROWS;
            self.len += 1;
            slot
        } else {
            let slot = self.start;
            self.start = (self.start + 1) % SCROLLBACK_ROWS;
            slot
        };
        let offset = slot * self.columns;
        self.cells[offset..offset + self.columns].copy_from_slice(cells);
    }

    fn row(&self, row: usize) -> &[Cell] {
        debug_assert!(row < self.len);
        let slot = (self.start + row) % SCROLLBACK_ROWS;
        let offset = slot * self.columns;
        &self.cells[offset..offset + self.columns]
    }
}

#[derive(Clone, Copy)]
struct Point {
    row: usize,
    column: usize,
}

struct Reflowed {
    cells: Vec<Cell>,
    columns: usize,
    rows: usize,
    cursor: Point,
    saved: Point,
}

impl Reflowed {
    fn row(&self, row: usize) -> &[Cell] {
        let offset = row * self.columns;
        &self.cells[offset..offset + self.columns]
    }
}

struct ReflowWriter {
    cells: Vec<Cell>,
    columns: usize,
    row: usize,
    column: usize,
    cursor: Option<(usize, usize)>,
    saved: Option<(usize, usize)>,
    blank: Cell,
}

impl ReflowWriter {
    fn new(blank: Cell, columns: usize) -> Option<Self> {
        let mut writer = Self {
            cells: Vec::new(),
            columns,
            row: 0,
            column: 0,
            cursor: None,
            saved: None,
            blank,
        };
        writer.add_row()?;
        Some(writer)
    }

    fn capture_cursor(&mut self) {
        self.cursor = Some((self.row, self.column));
    }

    fn capture_saved(&mut self) {
        self.saved = Some((self.row, self.column));
    }

    fn put(&mut self, mut cell: Cell) -> Option<()> {
        if self.column == self.columns {
            let marker = self.row * self.columns + self.columns - 1;
            self.cells[marker].reserved |= SOFT_WRAPPED_ROW;
            self.column = 0;
            self.advance_row()?;
        }
        cell.reserved &= !SOFT_WRAPPED_ROW;
        let index = self.row * self.columns + self.column;
        self.cells[index] = cell;
        self.column += 1;
        Some(())
    }

    fn hard_break(&mut self) -> Option<()> {
        self.column = 0;
        self.advance_row()
    }

    fn advance_row(&mut self) -> Option<()> {
        self.row += 1;
        self.add_row()
    }

    fn add_row(&mut self) -> Option<()> {
        self.cells.try_reserve_exact(self.columns).ok()?;
        self.cells
            .resize(self.cells.len().checked_add(self.columns)?, self.blank);
        Some(())
    }

    fn finish(self) -> Reflowed {
        let (cursor_row, cursor_column) = self.cursor.unwrap_or((self.row, self.column));
        let (saved_row, saved_column) = self.saved.unwrap_or((cursor_row, cursor_column));
        Reflowed {
            cells: self.cells,
            columns: self.columns,
            rows: self.row + 1,
            cursor: Point {
                row: cursor_row,
                column: cursor_column,
            },
            saved: Point {
                row: saved_row,
                column: saved_column,
            },
        }
    }
}

struct Source<'a> {
    history: Option<&'a History>,
    screen: Screen,
    columns: usize,
    rows: usize,
}

impl Source<'_> {
    fn history_rows(&self) -> usize {
        self.history.map_or(0, |history| history.len)
    }

    fn total_rows(&self) -> usize {
        self.history_rows() + self.rows
    }

    fn cell(&self, row: usize, column: usize) -> Cell {
        if row < self.history_rows() {
            return self.history.expect("history row").row(row)[column];
        }
        let screen_row = row - self.history_rows();
        // SAFETY: resize_screen iterates only total_rows() and source_columns, so the derived
        // screen row and column are inside the still-live source allocation.
        unsafe { *self.screen.cells.add(screen_row * self.columns + column) }
    }

    fn wrapped(&self, row: usize) -> bool {
        self.cell(row, self.columns - 1).reserved & SOFT_WRAPPED_ROW != 0
    }

    fn content_end(&self, row: usize) -> usize {
        for column in (0..self.columns).rev() {
            let cell = self.cell(row, column);
            let default_background = cell.reserved & BACKGROUND_INDEXED != 0
                && cell.reserved & BACKGROUND_INDEX_MASK == 0;
            if cell.codepoint != b' ' as u32 || cell.attributes != 0 || !default_background {
                return column + 1;
            }
        }
        0
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn resize_screen(
    source_history: Option<&History>,
    source: Screen,
    source_columns: usize,
    source_rows: usize,
    output: &mut Screen,
    output_columns: usize,
    output_rows: usize,
    mut output_history: Option<&mut History>,
) -> Option<()> {
    let input = Source {
        history: source_history,
        screen: source,
        columns: source_columns,
        rows: source_rows,
    };
    let history_rows = input.history_rows();
    let cursor_row = history_rows + source.row;
    let saved_row = history_rows + source.saved.row;
    // SAFETY: allocate_grid rejects zero-sized grids, so output.cells points at the first initialized
    // destination cell for the entire resize transaction.
    let blank = unsafe { *output.cells };
    let mut writer = ReflowWriter::new(blank, output_columns)?;
    // 1. Reconstruct logical lines from scrollback plus the complete visible screen, mapping both
    //    live and saved cursors while replacing old soft-wrap boundaries with the new width.
    for row in 0..input.total_rows() {
        let wrapped = input.wrapped(row);
        let mut end = if wrapped {
            source_columns
        } else {
            input.content_end(row)
        };
        if row == cursor_row {
            end = end.max(source.column);
        }
        if row == saved_row {
            end = end.max(source.saved.column);
        }
        for column in 0..end {
            if row == cursor_row && column == source.column {
                writer.capture_cursor();
            }
            if row == saved_row && column == source.saved.column {
                writer.capture_saved();
            }
            writer.put(input.cell(row, column))?;
        }
        if row == cursor_row && source.column >= end {
            writer.capture_cursor();
        }
        if row == saved_row && source.saved.column >= end {
            writer.capture_saved();
        }
        if !wrapped && row + 1 < input.total_rows() {
            writer.hard_break()?;
        }
    }
    let reflowed = writer.finish();
    // 2. Keep the cursor at its old visual row. Growing the terminal may pull preceding history
    //    into the new rows, while shrinking clamps the cursor instead of letting content below it
    //    scroll the cursor to row zero.
    let growth = output_rows.saturating_sub(source_rows);
    let desired_cursor_row = source
        .row
        .saturating_add(growth)
        .min(output_rows - 1)
        .min(reflowed.cursor.row);
    let viewport_start = reflowed.cursor.row.saturating_sub(desired_cursor_row);

    // 3. Split the reflowed sequence at the viewport boundary: preceding rows return to the bounded
    //    history ring and exactly output_rows rows become the new visible screen.
    if let Some(history) = output_history.as_mut() {
        history.clear();
        for row in 0..viewport_start {
            history.push_row(reflowed.row(row));
        }
    }
    for row in 0..output_rows {
        let source_row = viewport_start + row;
        if source_row >= reflowed.rows {
            break;
        }
        // SAFETY: both slices contain output_columns initialized cells and each destination row is
        // disjoint inside the freshly allocated output grid.
        unsafe {
            ptr::copy_nonoverlapping(
                reflowed.row(source_row).as_ptr(),
                output.cells.add(row * output_columns),
                output_columns,
            );
        }
    }
    output.row = reflowed.cursor.row.saturating_sub(viewport_start);
    output.column = reflowed.cursor.column;
    output.saved = source.saved;
    output.saved.row = reflowed
        .saved
        .row
        .saturating_sub(viewport_start)
        .min(output_rows - 1);
    output.saved.column = reflowed.saved.column.min(output_columns - 1);
    Some(())
}

pub(super) fn allocate_grid(
    columns: usize,
    rows: usize,
    foreground: u32,
    background: u32,
    reserved: u16,
) -> Option<(Screen, Screen, *mut DirtySpan)> {
    let count = columns.checked_mul(rows).filter(|count| *count != 0)?;
    let blank = Cell::blank(foreground, background, reserved);
    let primary = allocate_boxed(count, blank)?;
    let alternate = allocate_boxed(count, blank)?;
    let dirty = allocate_boxed(
        rows,
        DirtySpan {
            first: 0,
            end: columns as u32,
        },
    )?;
    Some((
        Screen {
            cells: primary,
            column: 0,
            row: 0,
            saved: SavedState::initial(),
        },
        Screen {
            cells: alternate,
            column: 0,
            row: 0,
            saved: SavedState::initial(),
        },
        dirty,
    ))
}

fn allocate_boxed<T: Copy>(length: usize, value: T) -> Option<*mut T> {
    let mut values = Vec::new();
    values.try_reserve_exact(length).ok()?;
    values.resize(length, value);
    Some(Box::into_raw(values.into_boxed_slice()).cast::<T>())
}

pub(super) fn free_grid(
    primary: Screen,
    alternate: Screen,
    dirty: *mut DirtySpan,
    columns: usize,
    rows: usize,
) {
    let count = columns.saturating_mul(rows);
    unsafe {
        if !primary.cells.is_null() {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                primary.cells,
                count,
            )));
        }
        if !alternate.cells.is_null() {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                alternate.cells,
                count,
            )));
        }
        if !dirty.is_null() {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                dirty, rows,
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(model: &mut Model, bytes: impl AsRef<[u8]>) {
        model.feed(bytes.as_ref(), |_| {});
    }

    fn resize(model: &mut Model, columns: usize, rows: usize) {
        let candidate = model
            .prepare_resize(columns, rows)
            .expect("resize allocation");
        model.commit_resize(candidate);
    }

    fn row_text(cells: &[Cell]) -> String {
        let end = cells
            .iter()
            .rposition(|cell| cell.codepoint != b' ' as u32)
            .map_or(0, |index| index + 1);
        cells[..end]
            .iter()
            .map(|cell| char::from_u32(cell.codepoint).expect("valid test cell"))
            .collect()
    }

    fn visible_rows(model: &Model, screen: Screen) -> Vec<String> {
        (0..model.rows)
            .map(|row| {
                let cells = unsafe {
                    core::slice::from_raw_parts(
                        screen.cells.add(row * model.columns),
                        model.columns,
                    )
                };
                row_text(cells)
            })
            .collect()
    }

    fn primary_transcript(model: &Model) -> String {
        let mut rows: Vec<String> = (0..model.history.len)
            .map(|row| row_text(model.history.row(row)))
            .collect();
        rows.extend(visible_rows(model, model.primary));
        rows.join("\n")
    }

    #[test]
    fn height_growth_pulls_scrollback_above_bottom_anchored_cursor() {
        let mut model = Model::new(24, 5).expect("model");
        for index in 0..12 {
            feed(&mut model, format!("line-{index:02}\r\n"));
        }
        assert_eq!(model.primary.row, 4);
        assert!(model.history.len >= 8);

        resize(&mut model, 24, 9);

        assert_eq!(model.primary.row, 8);
        assert_eq!(
            visible_rows(&model, model.primary),
            vec![
                "line-04", "line-05", "line-06", "line-07", "line-08", "line-09", "line-10",
                "line-11", "",
            ]
        );
    }

    #[test]
    fn content_below_cursor_cannot_relocate_continuing_output_to_top() {
        let mut model = Model::new(20, 8).expect("model");
        for index in 0..8 {
            feed(&mut model, format!("old-{index}\r\n"));
        }
        feed(&mut model, b"\x1b[3;1Hlive");
        assert_eq!(model.primary.row, 2);

        resize(&mut model, 20, 4);
        assert_eq!(model.primary.row, 2);
        feed(&mut model, b"\rcontinued\x1b[K");

        let rows = visible_rows(&model, model.primary);
        assert_ne!(rows[0], "continued");
        assert_eq!(rows[2], "continued");
    }

    #[test]
    fn apk_style_output_remains_chronological_across_resize() {
        let mut model = Model::new(72, 12).expect("model");
        feed(&mut model, b"OK: 5667 distinct packages available\r\n");
        feed(&mut model, b"/ # apk add nodejs\r\n");
        for index in 1..=10 {
            feed(
                &mut model,
                format!("({index}/17) Installing package-{index}\r\n"),
            );
        }

        resize(&mut model, 96, 20);
        for index in 11..=17 {
            feed(
                &mut model,
                format!("({index}/17) Installing package-{index}\r\n"),
            );
        }
        feed(&mut model, b"24% ################");

        let transcript = primary_transcript(&model);
        let mut previous = transcript.find("/ # apk add nodejs").expect("command");
        for index in 1..=17 {
            let marker = format!("({index}/17)");
            let position = transcript.find(&marker).expect("package marker");
            assert!(
                position > previous,
                "{marker} moved before preceding output after resize"
            );
            previous = position;
        }
        assert!(transcript[previous..].contains("24%"));
    }

    #[test]
    fn carriage_return_progress_overwrites_the_same_logical_row_after_resize() {
        let mut model = Model::new(32, 5).expect("model");
        for index in 0..7 {
            feed(&mut model, format!("package-{index}\r\n"));
        }
        feed(&mut model, b"24% ########");

        resize(&mut model, 48, 8);
        feed(&mut model, b"\r78% ################\x1b[K");

        let rows = visible_rows(&model, model.primary);
        assert_eq!(rows[model.primary.row], "78% ################");
        assert!(!primary_transcript(&model).contains("24%"));
    }

    #[test]
    fn alternate_screen_content_survives_resize_without_entering_scrollback() {
        let mut model = Model::new(20, 5).expect("model");
        feed(&mut model, b"primary\x1b[?1049halternate");

        resize(&mut model, 28, 7);

        assert!(model.alternate_active);
        assert!(
            visible_rows(&model, model.alternate)
                .iter()
                .any(|row| row.contains("alternate"))
        );
        assert_eq!(model.history.len, 0);
        feed(&mut model, b"\x1b[?1049l");
        assert!(
            visible_rows(&model, model.primary)
                .iter()
                .any(|row| row.contains("primary"))
        );
    }
}
