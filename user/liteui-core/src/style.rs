use crate::Rect;

/// Bounded semantic role consumed by compositor-owned interaction policy.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum NodeRole {
    #[default]
    Normal = 0,
    Window = 1,
    TitleBar = 2,
    Close = 3,
    Minimize = 4,
    Maximize = 5,
    Restore = 6,
    Action = 7,
    TextGrid = 8,
}

impl NodeRole {
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Normal),
            1 => Some(Self::Window),
            2 => Some(Self::TitleBar),
            3 => Some(Self::Close),
            4 => Some(Self::Minimize),
            5 => Some(Self::Maximize),
            6 => Some(Self::Restore),
            7 => Some(Self::Action),
            8 => Some(Self::TextGrid),
            _ => None,
        }
    }
}

/// Parent-relative positioning modes resolved by the Rust layout projection.
///
/// Pixels remain deterministic 26.6 values. Anchoring is intentionally small:
/// it covers desktop chrome resize without exposing a CSS parser or duplicating
/// viewport state in JavaScript.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct Anchors(u8);

impl Anchors {
    pub const NONE: Self = Self(0);
    pub const RIGHT: u8 = 1 << 0;
    pub const BOTTOM: u8 = 1 << 1;
    pub const STRETCH_WIDTH: u8 = 1 << 2;
    pub const STRETCH_HEIGHT: u8 = 1 << 3;
    pub const CENTER_X: u8 = 1 << 4;
    pub const CENTER_Y: u8 = 1 << 5;

    pub const fn from_bits(bits: u8) -> Option<Self> {
        let known = Self::RIGHT
            | Self::BOTTOM
            | Self::STRETCH_WIDTH
            | Self::STRETCH_HEIGHT
            | Self::CENTER_X
            | Self::CENTER_Y;
        let horizontal = (bits & Self::RIGHT != 0) as u8
            + (bits & Self::STRETCH_WIDTH != 0) as u8
            + (bits & Self::CENTER_X != 0) as u8;
        let vertical = (bits & Self::BOTTOM != 0) as u8
            + (bits & Self::STRETCH_HEIGHT != 0) as u8
            + (bits & Self::CENTER_Y != 0) as u8;
        if bits & !known == 0 && horizontal <= 1 && vertical <= 1 {
            Some(Self(bits))
        } else {
            None
        }
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub(crate) const fn contains(self, bit: u8) -> bool {
        self.0 & bit != 0
    }
}

/// Typed visual properties accepted by the first LiteUI ABI.
///
/// This intentionally is not a CSS object. The host compiler converts the
/// supported CSS subset to this bounded representation before guest execution.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub bounds: Rect,
    pub background: u32,
    pub border_color: u32,
    pub border_width: u8,
    pub visible: bool,
    pub anchors: Anchors,
    pub role: NodeRole,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            bounds: Rect::default(),
            background: 0,
            border_color: 0,
            border_width: 0,
            visible: true,
            anchors: Anchors::NONE,
            role: NodeRole::Normal,
        }
    }
}
