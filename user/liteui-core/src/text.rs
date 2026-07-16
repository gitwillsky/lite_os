/// Maximum UTF-8 bytes carried by one deterministic phase-one text run.
pub const MAX_TEXT_BYTES: usize = 24;

/// Inline, allocation-free text content owned by one retained node.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TextRun {
    bytes: [u8; MAX_TEXT_BYTES],
    length: u8,
    color: u32,
    bold: bool,
}

impl TextRun {
    pub fn try_new(bytes: &[u8], color: u32, bold: bool) -> Option<Self> {
        if bytes.is_empty()
            || bytes.len() > MAX_TEXT_BYTES
            || core::str::from_utf8(bytes).is_err()
            || bytes.contains(&0)
        {
            return None;
        }
        let mut owned = [0; MAX_TEXT_BYTES];
        owned[..bytes.len()].copy_from_slice(bytes);
        Some(Self {
            bytes: owned,
            length: bytes.len() as u8,
            color,
            bold,
        })
    }

    pub fn bytes(self) -> [u8; MAX_TEXT_BYTES] {
        self.bytes
    }

    pub fn length(self) -> usize {
        usize::from(self.length)
    }

    pub fn color(self) -> u32 {
        self.color
    }

    pub fn bold(self) -> bool {
        self.bold
    }
}
