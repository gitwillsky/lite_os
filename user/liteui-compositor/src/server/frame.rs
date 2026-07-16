use liteui_core::GridCell;

use super::{
    GRID_CELL_BYTES, HEADER_BYTES, MAX_OPERATIONS, MAX_PAYLOAD_BYTES, OPERATION_BYTES,
    TEXT_GRID_CAPACITY,
};

pub(super) struct Header {
    pub(super) epoch: u64,
    pub(super) sequence: u64,
    pub(super) payload_length: usize,
    pub(super) kind: FrameKind,
}

pub(super) enum FrameKind {
    Ui { operations: usize },
    Grid(GridHeader),
}

pub(super) struct GridHeader {
    pub(super) columns: u16,
    pub(super) rows: u16,
    pub(super) cursor: Option<(u16, u16)>,
    pub(super) reverse: bool,
    pub(super) blink_visible: bool,
}

impl Header {
    pub(super) fn decode(bytes: &[u8]) -> Result<Option<Self>, ()> {
        if bytes.len() < HEADER_BYTES {
            return Ok(None);
        }
        if read_u16(bytes, 4)? != 1 || read_u16(bytes, 6)? as usize != HEADER_BYTES {
            return Err(());
        }
        let kind = if &bytes[..4] == b"LUI1" {
            let operations = read_u32(bytes, 24)? as usize;
            let payload_length = read_u32(bytes, 28)? as usize;
            if operations > MAX_OPERATIONS
                || payload_length > MAX_PAYLOAD_BYTES
                || payload_length != operations.checked_mul(OPERATION_BYTES).ok_or(())?
                || read_u32(bytes, 32)? != 0
                || read_u32(bytes, 36)? != 0
            {
                return Err(());
            }
            FrameKind::Ui { operations }
        } else if &bytes[..4] == b"LUG1" {
            let columns = read_u16(bytes, 24)?;
            let rows = read_u16(bytes, 26)?;
            let payload_length = read_u32(bytes, 28)? as usize;
            let cursor_column = read_u16(bytes, 32)?;
            let cursor_row = read_u16(bytes, 34)?;
            let flags = read_u16(bytes, 36)?;
            let count = usize::from(columns)
                .checked_mul(usize::from(rows))
                .ok_or(())?;
            if count == 0
                || count > TEXT_GRID_CAPACITY
                || payload_length != count.checked_mul(GRID_CELL_BYTES).ok_or(())?
                || payload_length > MAX_PAYLOAD_BYTES
                || flags & !3 != 0
                || read_u16(bytes, 38)? != 0
                || (cursor_column == u16::MAX) != (cursor_row == u16::MAX)
                || cursor_column != u16::MAX && (cursor_column >= columns || cursor_row >= rows)
            {
                return Err(());
            }
            FrameKind::Grid(GridHeader {
                columns,
                rows,
                cursor: (cursor_column != u16::MAX).then_some((cursor_row, cursor_column)),
                reverse: flags & 1 != 0,
                blink_visible: flags & 2 != 0,
            })
        } else {
            return Err(());
        };
        let payload_length = match &kind {
            FrameKind::Ui { operations } => *operations * OPERATION_BYTES,
            FrameKind::Grid(grid) => {
                usize::from(grid.columns) * usize::from(grid.rows) * GRID_CELL_BYTES
            }
        };
        Ok(Some(Self {
            epoch: read_u64(bytes, 8)?,
            sequence: read_u64(bytes, 16)?,
            payload_length,
            kind,
        }))
    }
}

pub(super) fn decode_grid_cell(bytes: &[u8]) -> Result<GridCell, ()> {
    if bytes.len() != GRID_CELL_BYTES {
        return Err(());
    }
    Ok(GridCell {
        codepoint: read_u32(bytes, 0)?,
        foreground: read_u32(bytes, 4)?,
        background: read_u32(bytes, 8)?,
        attributes: read_u16(bytes, 12)?,
        reserved: read_u16(bytes, 14)?,
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ()> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or(())?
            .try_into()
            .map_err(|_| ())?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ()> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or(())?
            .try_into()
            .map_err(|_| ())?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, ()> {
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..offset + 8)
            .ok_or(())?
            .try_into()
            .map_err(|_| ())?,
    ))
}
