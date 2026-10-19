use super::*;

impl Model {
    pub(super) fn reset_style(&mut self) {
        self.foreground_index = Some(7);
        self.background_index = Some(0);
        self.foreground = self.palette[7];
        self.background = self.palette[0];
        self.attributes = 0;
    }

    pub(super) fn sgr(&mut self) {
        let mut index = 0;
        while index < self.parameter_count {
            let value = self.parameters[index];
            match value {
                0 => self.reset_style(),
                1 => self.attributes = self.attributes & !ATTR_DIM | ATTR_BOLD,
                2 => self.attributes = self.attributes & !ATTR_BOLD | ATTR_DIM,
                4 => self.attributes |= ATTR_UNDERLINE,
                5 => self.attributes |= ATTR_BLINK,
                7 => self.attributes |= ATTR_INVERSE,
                8 => self.attributes |= ATTR_HIDDEN,
                10 => self.direct_graphics = false,
                11 => self.direct_graphics = true,
                22 => self.attributes &= !(ATTR_BOLD | ATTR_DIM),
                24 => self.attributes &= !ATTR_UNDERLINE,
                25 => self.attributes &= !ATTR_BLINK,
                27 => self.attributes &= !ATTR_INVERSE,
                28 => self.attributes &= !ATTR_HIDDEN,
                30..=37 => self.set_foreground_index((value - 30) as usize),
                40..=47 => self.set_background_index((value - 40) as usize),
                90..=97 => self.set_foreground_index((value - 90 + 8) as usize),
                100..=107 => self.set_background_index((value - 100 + 8) as usize),
                39 => self.set_foreground_index(7),
                49 => self.set_background_index(0),
                38 | 48 => {
                    let foreground = value == 38;
                    if self.parameters.get(index + 1) == Some(&5)
                        && index + 2 < self.parameter_count
                    {
                        let palette_index = self.parameters[index + 2];
                        let color = if palette_index < 16 {
                            self.palette[palette_index as usize]
                        } else {
                            color256(palette_index)
                        };
                        if foreground {
                            self.foreground = color;
                            self.foreground_index =
                                (palette_index < 16).then_some(palette_index as u8);
                        } else {
                            self.background = color;
                            self.background_index =
                                (palette_index < 16).then_some(palette_index as u8);
                        }
                        index += 2;
                    } else if self.parameters.get(index + 1) == Some(&2)
                        && index + 4 < self.parameter_count
                    {
                        let color = rgb(
                            self.parameters[index + 2],
                            self.parameters[index + 3],
                            self.parameters[index + 4],
                        );
                        if foreground {
                            self.foreground = color;
                            self.foreground_index = None;
                        } else {
                            self.background = color;
                            self.background_index = None;
                        }
                        index += 4;
                    }
                }
                _ => {}
            }
            index += 1;
        }
    }

    pub(super) fn blank_cell(&self) -> Cell {
        Cell::blank(
            self.foreground,
            self.background,
            style_indices(self.foreground_index, self.background_index),
        )
    }

    pub(super) fn set_foreground_index(&mut self, index: usize) {
        let index = index.min(15);
        self.foreground_index = Some(index as u8);
        self.foreground = self.palette[index];
    }

    pub(super) fn set_background_index(&mut self, index: usize) {
        let index = index.min(15);
        self.background_index = Some(index as u8);
        self.background = self.palette[index];
    }

    pub(super) fn set_palette(&mut self, index: usize, color: u32) {
        if index >= self.palette.len() {
            return;
        }
        self.palette[index] = color;
        for screen in [self.primary, self.alternate] {
            for cell_index in 0..self.columns * self.rows {
                let cell = unsafe { &mut *screen.cells.add(cell_index) };
                if cell.reserved & FOREGROUND_INDEXED != 0
                    && usize::from((cell.reserved & FOREGROUND_INDEX_MASK) as u8) == index
                {
                    cell.foreground = color;
                }
                if cell.reserved & BACKGROUND_INDEXED != 0
                    && usize::from(((cell.reserved & BACKGROUND_INDEX_MASK) >> 4) as u8) == index
                {
                    cell.background = color;
                }
            }
        }
        if self.foreground_index == Some(index as u8) {
            self.foreground = color;
        }
        if self.background_index == Some(index as u8) {
            self.background = color;
        }
        self.mark_all();
    }

    pub(super) fn reset_palette(&mut self) {
        for (index, color) in DEFAULT_COLORS.into_iter().enumerate() {
            self.set_palette(index, color);
        }
    }
}

pub(super) fn rgb(red: u32, green: u32, blue: u32) -> u32 {
    red.min(255) << 16 | green.min(255) << 8 | blue.min(255)
}

pub(super) fn decimal(mut value: usize, output: &mut [u8]) -> usize {
    let mut reversed = [0u8; 20];
    let mut length = 0;
    loop {
        reversed[length] = b'0' + (value % 10) as u8;
        length += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for index in 0..length {
        output[index] = reversed[length - index - 1];
    }
    length
}

fn color256(index: u32) -> u32 {
    match index {
        0..=15 => DEFAULT_COLORS[index as usize],
        16..=231 => {
            let value = index - 16;
            let component = |value: u32| if value == 0 { 0 } else { 55 + value * 40 };
            rgb(
                component(value / 36),
                component(value / 6 % 6),
                component(value % 6),
            )
        }
        232..=255 => {
            let value = 8 + (index - 232) * 10;
            rgb(value, value, value)
        }
        _ => 0x00cbd5e1,
    }
}

pub(super) fn style_indices(foreground: Option<u8>, background: Option<u8>) -> u16 {
    let foreground = foreground.map_or(0, |index| {
        FOREGROUND_INDEXED | u16::from(index) & FOREGROUND_INDEX_MASK
    });
    let background = background.map_or(0, |index| {
        BACKGROUND_INDEXED | (u16::from(index) << 4) & BACKGROUND_INDEX_MASK
    });
    foreground | background
}

pub(super) fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
