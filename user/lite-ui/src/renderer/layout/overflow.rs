//! CSS `overflow` axis coupling and taffy lowering.

use taffy::Overflow;

use crate::style::Computed;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OverflowMode {
    Visible,
    Clip,
    Hidden,
    Auto,
    Scroll,
}

impl OverflowMode {
    pub(crate) fn clips(self) -> bool {
        self != Self::Visible
    }

    pub(crate) fn scrolls(self) -> bool {
        matches!(self, Self::Auto | Self::Scroll)
    }

    pub(crate) fn taffy(self) -> Overflow {
        match self {
            Self::Visible => Overflow::Visible,
            Self::Clip => Overflow::Clip,
            Self::Hidden => Overflow::Hidden,
            Self::Auto | Self::Scroll => Overflow::Scroll,
        }
    }
}

pub(crate) fn overflow_modes(computed: &Computed) -> (OverflowMode, OverflowMode) {
    let mut shorthand = computed
        .get("overflow")
        .unwrap_or("visible")
        .split_whitespace();
    let first = shorthand.next().unwrap_or("visible");
    let second = shorthand.next().unwrap_or(first);
    let mut x = computed.get("overflow-x").unwrap_or(first);
    let mut y = computed.get("overflow-y").unwrap_or(second);

    // CSS Overflow 3 computes a visible axis to auto (and clip to hidden) when
    // the other axis establishes a scroll container. Missing this coupling
    // lets one axis leak out of a container that is scrollable on the other.
    let x_contained = !matches!(x, "visible" | "clip");
    let y_contained = !matches!(y, "visible" | "clip");
    if y_contained {
        x = match x {
            "visible" => "auto",
            "clip" => "hidden",
            value => value,
        };
    }
    if x_contained {
        y = match y {
            "visible" => "auto",
            "clip" => "hidden",
            value => value,
        };
    }

    (overflow_mode(x), overflow_mode(y))
}

fn overflow_mode(value: &str) -> OverflowMode {
    match value {
        "clip" => OverflowMode::Clip,
        "hidden" => OverflowMode::Hidden,
        "auto" => OverflowMode::Auto,
        "scroll" => OverflowMode::Scroll,
        _ => OverflowMode::Visible,
    }
}
