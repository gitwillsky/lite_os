use alloc::{boxed::Box, vec::Vec};

pub const ATTR_BOLD: u16 = 1 << 0;
pub const ATTR_DIM: u16 = 1 << 1;
pub const ATTR_UNDERLINE: u16 = 1 << 2;
pub const ATTR_INVERSE: u16 = 1 << 3;
pub const ATTR_HIDDEN: u16 = 1 << 4;
pub const ATTR_BLINK: u16 = 1 << 5;
const ATTRIBUTE_MASK: u16 =
    ATTR_BOLD | ATTR_DIM | ATTR_UNDERLINE | ATTR_INVERSE | ATTR_HIDDEN | ATTR_BLINK;

/// Backend-neutral terminal cell copied across the TextGrid transaction seam.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GridCell {
    pub codepoint: u32,
    pub foreground: u32,
    pub background: u32,
    pub attributes: u16,
    pub reserved: u16,
}

const _: () = assert!(core::mem::size_of::<GridCell>() == 16);

impl GridCell {
    const EMPTY: Self = Self {
        codepoint: b' ' as u32,
        foreground: 0x00cbd5e1,
        background: 0x00101418,
        attributes: 0,
        reserved: 0,
    };
}

struct State {
    cells: Box<[GridCell]>,
    columns: usize,
    rows: usize,
    cursor: Option<(usize, usize)>,
    reverse: bool,
    blink_visible: bool,
}

/// Complete candidate supplied by the terminal service for one atomic publication.
pub struct GridUpdate<'a> {
    pub epoch: u64,
    pub sequence: u64,
    pub columns: usize,
    pub rows: usize,
    pub cursor: Option<(usize, usize)>,
    pub reverse: bool,
    pub blink_visible: bool,
    pub cells: &'a [GridCell],
}

/// Read-only frame view of the currently published grid.
#[derive(Clone, Copy)]
pub struct GridSnapshot<'a> {
    cells: &'a [GridCell],
    columns: usize,
    rows: usize,
    cursor: Option<(usize, usize)>,
    reverse: bool,
    blink_visible: bool,
}

impl GridSnapshot<'_> {
    pub fn columns(self) -> usize {
        self.columns
    }

    pub fn rows(self) -> usize {
        self.rows
    }

    pub fn cursor(self) -> Option<(usize, usize)> {
        self.cursor
    }

    pub fn reverse(self) -> bool {
        self.reverse
    }

    pub fn blink_visible(self) -> bool {
        self.blink_visible
    }

    pub fn cell(self, row: usize, column: usize) -> Option<GridCell> {
        (row < self.rows && column < self.columns).then(|| self.cells[row * self.columns + column])
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TextGridError {
    OutOfMemory,
    Capacity,
    WrongEpoch,
    WrongSequence,
    InvalidCell,
    InvalidCursor,
}

/// Fixed-capacity, two-state terminal resource with failure-atomic publication.
///
/// Both owners are allocated before the display loop. Commit validates and copies
/// into the invisible owner, then swaps once; render therefore never observes a
/// partially decoded PTY frame and never allocates.
pub struct TextGrid {
    epoch: u64,
    next_sequence: u64,
    active: State,
    staging: State,
}

impl TextGrid {
    pub fn try_new(epoch: u64, capacity: usize) -> Result<Self, TextGridError> {
        if capacity == 0 {
            return Err(TextGridError::Capacity);
        }
        Ok(Self {
            epoch,
            next_sequence: 1,
            active: State::try_new(capacity)?,
            staging: State::try_new(capacity)?,
        })
    }

    pub fn try_clone(&self) -> Result<Self, TextGridError> {
        let capacity = self.active.cells.len();
        let mut active = State::try_new(capacity)?;
        active.copy_from(&self.active);
        let mut staging = State::try_new(capacity)?;
        staging.copy_from(&active);
        Ok(Self {
            epoch: self.epoch,
            next_sequence: self.next_sequence,
            active,
            staging,
        })
    }

    pub fn commit(&mut self, update: GridUpdate<'_>) -> Result<(), TextGridError> {
        if update.epoch != self.epoch {
            return Err(TextGridError::WrongEpoch);
        }
        if update.sequence != self.next_sequence {
            return Err(TextGridError::WrongSequence);
        }
        let count = update
            .columns
            .checked_mul(update.rows)
            .ok_or(TextGridError::Capacity)?;
        if update.columns == 0
            || update.rows == 0
            || count > self.staging.cells.len()
            || update.cells.len() != count
        {
            return Err(TextGridError::Capacity);
        }
        if update
            .cursor
            .is_some_and(|(row, column)| row >= update.rows || column >= update.columns)
        {
            return Err(TextGridError::InvalidCursor);
        }
        for cell in update.cells {
            if char::from_u32(cell.codepoint).is_none()
                || cell.attributes & !ATTRIBUTE_MASK != 0
                || cell.reserved != 0
            {
                return Err(TextGridError::InvalidCell);
            }
        }
        let next = self
            .next_sequence
            .checked_add(1)
            .ok_or(TextGridError::WrongSequence)?;
        self.staging.cells[..count].copy_from_slice(update.cells);
        self.staging.columns = update.columns;
        self.staging.rows = update.rows;
        self.staging.cursor = update.cursor;
        self.staging.reverse = update.reverse;
        self.staging.blink_visible = update.blink_visible;
        core::mem::swap(&mut self.active, &mut self.staging);
        self.next_sequence = next;
        Ok(())
    }

    pub fn reset(&mut self, epoch: u64) {
        self.epoch = epoch;
        self.next_sequence = 1;
        self.active.clear();
        self.staging.clear();
    }

    pub fn snapshot(&self) -> Option<GridSnapshot<'_>> {
        let count = self.active.columns.checked_mul(self.active.rows)?;
        (count != 0).then_some(GridSnapshot {
            cells: &self.active.cells[..count],
            columns: self.active.columns,
            rows: self.active.rows,
            cursor: self.active.cursor,
            reverse: self.active.reverse,
            blink_visible: self.active.blink_visible,
        })
    }
}

impl State {
    fn try_new(capacity: usize) -> Result<Self, TextGridError> {
        let mut cells = Vec::new();
        cells
            .try_reserve_exact(capacity)
            .map_err(|_| TextGridError::OutOfMemory)?;
        cells.resize(capacity, GridCell::EMPTY);
        Ok(Self {
            cells: cells.into_boxed_slice(),
            columns: 0,
            rows: 0,
            cursor: None,
            reverse: false,
            blink_visible: true,
        })
    }

    fn copy_from(&mut self, source: &Self) {
        self.cells.copy_from_slice(&source.cells);
        self.columns = source.columns;
        self.rows = source.rows;
        self.cursor = source.cursor;
        self.reverse = source.reverse;
        self.blink_visible = source.blink_visible;
    }

    fn clear(&mut self) {
        self.columns = 0;
        self.rows = 0;
        self.cursor = None;
        self.reverse = false;
        self.blink_visible = true;
    }
}
