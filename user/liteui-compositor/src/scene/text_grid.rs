use liteui_core::{GridUpdate, NodeRole};

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
        self.text_grid.commit(update).map_err(|_| ())?;
        Ok(self.text_grid_bounds().map_or(Damage::EMPTY, Damage::one))
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
