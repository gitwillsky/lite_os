//! Helper screen projection from fixed protocol cells into React text runs.

use std::io;

use serde::Serialize;

use super::{APPLICATION_CURSOR_KEYS, cursor_appearance, invalid, read_u16, read_u32};

// Cell attribute bits mirror `terminal-session/src/model.rs`; underline, dim
// and blink have no raster in the fixed-cell atlas and are rendered as normal
// text.
pub(super) const ATTR_BOLD: u16 = 1 << 0;
pub(super) const ATTR_INVERSE: u16 = 1 << 3;
pub(super) const ATTR_HIDDEN: u16 = 1 << 4;
// 与 terminal-session 的 cell ABI 一致；缺失时前端会把宽字符尾随格重复投影成文本。
pub(super) const WIDE_CONTINUATION: u16 = 1 << 12;

/// One maximal same-style cell run inside one screen row.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(super) struct Run {
    pub(super) text: String,
    pub(super) columns: usize,
    pub(super) fg: u32,
    pub(super) bg: u32,
    pub(super) bold: bool,
}

/// Latest decoded helper screen: per-row style runs, the `(column, row)`
/// cursor and the current default colors.
#[derive(Default)]
pub(super) struct ScreenState {
    pub(super) rows: Vec<Vec<Run>>,
    pub(super) cursor: (u16, u16),
    pub(super) cursor_style: u16,
    pub(super) foreground: u32,
    pub(super) background: u32,
    pub(super) application_cursor_keys: bool,
}

impl ScreenState {
    pub(super) fn apply_update(&mut self, payload: &[u8]) -> io::Result<()> {
        if payload.len() < 24 {
            return Err(invalid("terminal update header truncated"));
        }
        let columns = read_u16(payload, 0)? as usize;
        let rows = read_u16(payload, 2)? as usize;
        // Header order pins `columns, rows, cursor_column, cursor_row`; the
        // helper writer in terminal-session emits the same sequence.
        self.cursor = (read_u16(payload, 4)?, read_u16(payload, 6)?);
        let dirty = read_u16(payload, 8)? as usize;
        let cursor_style = read_u16(payload, 10)?;
        if columns == 0 || rows == 0 || cursor_appearance(cursor_style).is_none() {
            return Err(invalid("terminal update geometry invalid"));
        }
        self.cursor_style = cursor_style;
        self.foreground = read_u32(payload, 12)?;
        self.background = read_u32(payload, 16)?;
        let input_modes = read_u32(payload, 20)?;
        if input_modes & !APPLICATION_CURSOR_KEYS != 0 {
            return Err(invalid("terminal input mode invalid"));
        }
        self.application_cursor_keys = input_modes & APPLICATION_CURSOR_KEYS != 0;
        if self.rows.len() != rows {
            self.rows = vec![Vec::new(); rows];
        }
        let mut offset = 24usize;
        for _ in 0..dirty {
            let row = read_u16(payload, offset)? as usize;
            if row >= rows || read_u16(payload, offset + 2)? != 0 {
                return Err(invalid("terminal dirty row invalid"));
            }
            offset += 4;
            let bytes = payload
                .get(
                    offset
                        ..offset
                            .checked_add(columns * 16)
                            .ok_or_else(|| invalid("terminal row overflow"))?,
                )
                .ok_or_else(|| invalid("terminal row truncated"))?;
            self.rows[row] = runs(bytes, self.background);
            offset += columns * 16;
        }
        if offset != payload.len() {
            return Err(invalid("terminal update has trailing bytes"));
        }
        Ok(())
    }
}

/// Collapses one full-width cell row into same-style runs and drops the
/// trailing invisible tail: whole runs of spaces on the default background
/// paint exactly the container color, so sending them would only grow the
/// bridge payload.
pub(super) fn runs(bytes: &[u8], default_background: u32) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for cell in bytes.as_chunks::<16>().0 {
        let mut codepoint = u32::from_le_bytes(cell[0..4].try_into().expect("cell codepoint"));
        let mut fg = u32::from_le_bytes(cell[4..8].try_into().expect("cell foreground"));
        let mut bg = u32::from_le_bytes(cell[8..12].try_into().expect("cell background"));
        let attributes = u16::from_le_bytes(cell[12..14].try_into().expect("cell attributes"));
        let metadata = u16::from_le_bytes(cell[14..16].try_into().expect("cell metadata"));
        if attributes & ATTR_HIDDEN != 0 {
            codepoint = b' ' as u32;
        }
        if attributes & ATTR_INVERSE != 0 {
            std::mem::swap(&mut fg, &mut bg);
        }
        let bold = attributes & ATTR_BOLD != 0;
        let character = (metadata & WIDE_CONTINUATION == 0)
            .then(|| char::from_u32(codepoint).unwrap_or('\u{fffd}'));
        match runs.last_mut() {
            Some(run) if run.fg == fg && run.bg == bg && run.bold == bold => {
                run.columns += 1;
                if let Some(character) = character {
                    run.text.push(character);
                }
            }
            _ => runs.push(Run {
                text: character.into_iter().collect(),
                columns: 1,
                fg,
                bg,
                bold,
            }),
        }
    }
    while runs
        .last()
        .is_some_and(|run| run.bg == default_background && run.text.chars().all(|c| c == ' '))
    {
        runs.pop();
    }
    runs
}
