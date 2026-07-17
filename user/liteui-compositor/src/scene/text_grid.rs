use liteui_core::{ATTR_BLINK, GridSnapshot, GridUpdate, NodeRole};

use super::{Damage, Rect, Scene, contains};

pub const TEXT_GRID_CAPACITY: usize = 16_384;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct GridConfiguration {
    pub columns: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

#[derive(Clone, Copy)]
pub struct TerminalPointer {
    pub button: u8,
    pub pressed: bool,
    pub column: u16,
    pub row: u16,
}

impl Scene {
    pub fn publish_grid(&mut self, update: GridUpdate<'_>) -> Result<Damage, ()> {
        let damage = self
            .text_grid_bounds()
            .map(|bounds| changed_grid_damage(bounds, self.text_grid.snapshot(), &update))
            .unwrap_or(Damage::EMPTY);
        self.text_grid.commit(update).map_err(|_| ())?;
        Ok(damage)
    }

    pub fn deactivate_grid(&mut self) -> Damage {
        let damage = self.text_grid_bounds().map_or(Damage::EMPTY, Damage::one);
        self.text_grid.reset(1);
        damage
    }

    pub fn grid_configuration(&self) -> Option<GridConfiguration> {
        let bounds = self.text_grid_bounds()?;
        let columns = (bounds.x2.saturating_sub(bounds.x1) / 16).max(1);
        let rows = (bounds.y2.saturating_sub(bounds.y1) / 32).max(1);
        Some(GridConfiguration {
            columns: u16::try_from(columns).ok()?,
            rows: u16::try_from(rows).ok()?,
            pixel_width: u16::try_from(columns.checked_mul(16)?).ok()?,
            pixel_height: u16::try_from(rows.checked_mul(32)?).ok()?,
        })
    }

    pub fn terminal_focused(&self) -> bool {
        self.text_grid.snapshot().is_some()
            && self
                .windows
                .focused_contains(NodeRole::TextGrid, self.client_draw_list.as_slice())
    }

    pub fn terminal_pointer(&self, button: u8, pressed: bool) -> Option<TerminalPointer> {
        if !self.terminal_focused() {
            return None;
        }
        let bounds = self.text_grid_bounds()?;
        if !contains(bounds, self.pointer.x, self.pointer.y) {
            return None;
        }
        Some(TerminalPointer {
            button,
            pressed,
            column: u16::try_from((self.pointer.x - bounds.x1) / 16).ok()?,
            row: u16::try_from((self.pointer.y - bounds.y1) / 32).ok()?,
        })
    }

    fn text_grid_bounds(&self) -> Option<Rect> {
        self.client_draw_list
            .as_slice()
            .iter()
            .find_map(|primitive| {
                let info = primitive.info();
                (info.role == NodeRole::TextGrid)
                    .then(|| self.windows.project(info))
                    .flatten()
            })
    }
}

fn changed_grid_damage(
    bounds: Rect,
    previous: Option<GridSnapshot<'_>>,
    update: &GridUpdate<'_>,
) -> Damage {
    let Some(cell_count) = update.columns.checked_mul(update.rows) else {
        return Damage::one(bounds);
    };
    if cell_count == 0 || update.cells.len() != cell_count {
        return Damage::one(bounds);
    }
    let Some(previous) = previous else {
        return Damage::one(bounds);
    };
    if previous.columns() != update.columns
        || previous.rows() != update.rows
        || previous.reverse() != update.reverse
    {
        return Damage::one(bounds);
    }

    let blink_changed = previous.blink_visible() != update.blink_visible;
    let mut damage = Damage::EMPTY;
    for row in 0..update.rows {
        let mut first_changed = None;
        for column in 0..=update.columns {
            let changed = if column == update.columns {
                false
            } else {
                let Some(old) = previous.cell(row, column) else {
                    return Damage::one(bounds);
                };
                let new = update.cells[row * update.columns + column];
                old != new || blink_changed && (old.attributes | new.attributes) & ATTR_BLINK != 0
            };
            match (first_changed, changed) {
                (None, true) => first_changed = Some(column),
                (Some(first), false) => {
                    damage.push(grid_span(bounds, row, first, column));
                    first_changed = None;
                }
                _ => {}
            }
        }
    }

    // The cursor is rendered as part of its cell; both positions must be
    // repainted or the old underline remains visible after a move.
    if previous.cursor() != update.cursor {
        if let Some((row, column)) = previous.cursor() {
            damage.push(grid_span(bounds, row, column, column + 1));
        }
        if let Some((row, column)) = update.cursor {
            damage.push(grid_span(bounds, row, column, column + 1));
        }
    }
    damage
}

fn grid_span(bounds: Rect, row: usize, first_column: usize, end_column: usize) -> Rect {
    Rect {
        x1: bounds.x1.saturating_add(first_column.saturating_mul(16)),
        y1: bounds.y1.saturating_add(row.saturating_mul(32)),
        x2: bounds
            .x1
            .saturating_add(end_column.saturating_mul(16))
            .min(bounds.x2),
        y2: bounds
            .y1
            .saturating_add(row.saturating_add(1).saturating_mul(32))
            .min(bounds.y2),
    }
}
