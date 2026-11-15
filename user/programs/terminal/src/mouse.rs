//! xterm mouse reports for the standalone terminal's Canvas.
use crate::model::TermMode;
use keyboard::Modifiers;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub row: usize,
    pub column: usize,
    pub right: bool,
}

#[derive(Default)]
pub struct Mouse {
    buttons: u8,
    cell: Option<(usize, usize)>,
}

impl Mouse {
    pub fn cancel(&mut self) {
        self.buttons = 0;
        self.cell = None;
    }

    pub fn button(
        &mut self,
        mode: TermMode,
        mods: Modifiers,
        position: Position,
        button: u8,
        pressed: bool,
    ) -> Option<Vec<u8>> {
        let bit = 1 << button;
        if pressed {
            if !reporting(mode, mods) {
                return None;
            }
            self.buttons |= bit;
        } else {
            if self.buttons & bit == 0 {
                return None;
            }
            self.buttons &= !bit;
            if !mode.intersects(TermMode::MOUSE_MODE) {
                return None;
            }
        }
        report(mode, mods, position, button, !pressed)
    }

    pub fn motion(
        &mut self,
        mode: TermMode,
        mods: Modifiers,
        position: Position,
    ) -> Option<Vec<u8>> {
        let cell = (position.row, position.column);
        let previous = self.cell.replace(cell);
        if previous == Some(cell) || !reporting(mode, mods) {
            return None;
        }
        let wants_motion = mode.contains(TermMode::MOUSE_MOTION)
            || (mode.contains(TermMode::MOUSE_DRAG) && self.buttons != 0);
        let button = self.buttons.trailing_zeros().min(3) as u8;
        wants_motion
            .then(|| report(mode, mods, position, button + 32, false))
            .flatten()
    }

    pub fn wheel(
        mode: TermMode,
        mods: Modifiers,
        position: Position,
        vertical: i32,
        horizontal: i32,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (amount, negative, positive) in [(vertical, 64, 65), (horizontal, 66, 67)] {
            let button = if amount < 0 { negative } else { positive };
            if let Some(report) = report(mode, mods, position, button, false) {
                for _ in 0..amount.unsigned_abs().min(64) {
                    bytes.extend_from_slice(&report);
                }
            }
        }
        bytes
    }
}

pub fn reporting(mode: TermMode, mods: Modifiers) -> bool {
    mode.intersects(TermMode::MOUSE_MODE) && !mods.shift
}

fn report(
    mode: TermMode,
    mods: Modifiers,
    position: Position,
    button: u8,
    released: bool,
) -> Option<Vec<u8>> {
    let flags = 4 * u8::from(mods.shift) + 8 * u8::from(mods.alt) + 16 * u8::from(mods.control);
    let Position { row, column, .. } = position;
    if mode.contains(TermMode::SGR_MOUSE) {
        let end = if released { 'm' } else { 'M' };
        return Some(
            format!("\x1b[<{};{};{}{end}", button + flags, column + 1, row + 1).into_bytes(),
        );
    }
    let utf8 = mode.contains(TermMode::UTF8_MOUSE);
    let limit = if utf8 { 2015 } else { 223 };
    if row >= limit || column >= limit {
        return None;
    }
    let code = if released { 3 } else { button };
    let mut bytes = vec![b'\x1b', b'[', b'M', 32 + code + flags];
    for value in [column + 33, row + 33] {
        if utf8 {
            let symbol = char::from_u32(value as u32).expect("bounded mouse coordinate");
            bytes.extend_from_slice(symbol.encode_utf8(&mut [0; 4]).as_bytes());
        } else {
            bytes.push(value as u8);
        }
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sgr_mouse_keeps_button_identity_on_release_and_reports_drag_in_cells() {
        let mode = TermMode::SGR_MOUSE | TermMode::MOUSE_DRAG;
        let mods = Modifiers::default();
        let pos = Position {
            row: 3,
            column: 2,
            right: false,
        };
        let mut mouse = Mouse::default();
        assert_eq!(mouse.motion(mode, mods, pos), None);
        assert_eq!(
            mouse.button(mode, mods, pos, 2, true).as_deref(),
            Some(&b"\x1b[<2;3;4M"[..])
        );
        assert_eq!(mouse.motion(mode, mods, pos), None);
        let next = Position { column: 4, ..pos };
        assert_eq!(
            mouse.motion(mode, mods, next).as_deref(),
            Some(&b"\x1b[<34;5;4M"[..])
        );
        assert_eq!(
            mouse.button(mode, mods, next, 2, false).as_deref(),
            Some(&b"\x1b[<2;5;4m"[..])
        );
    }
}
