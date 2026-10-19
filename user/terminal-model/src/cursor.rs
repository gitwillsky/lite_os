use super::*;

#[derive(Clone, Copy)]
#[repr(u16)]
pub(super) enum CursorStyle {
    BlinkingBlock = 1,
    SteadyBlock = 2,
    BlinkingUnderline = 3,
    SteadyUnderline = 4,
    BlinkingBar = 5,
    SteadyBar = 6,
}

impl Model {
    pub(super) fn finish_cursor_style(&mut self, final_byte: u8) {
        if final_byte == b'q' {
            self.cursor_style = match self.parameters[0] {
                0 | 1 => CursorStyle::BlinkingBlock,
                2 => CursorStyle::SteadyBlock,
                3 => CursorStyle::BlinkingUnderline,
                4 => CursorStyle::SteadyUnderline,
                5 => CursorStyle::BlinkingBar,
                6 => CursorStyle::SteadyBar,
                _ => self.cursor_style,
            };
        }
        self.parser = ParserState::Ground;
    }
}
