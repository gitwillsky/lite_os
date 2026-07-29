use taffy::Style;

use crate::style::Computed;

use super::{dimension, number};

/// Lowers the three CSS Flexbox longhands after shorthand expansion.
pub(super) fn apply(computed: &Computed, style: &mut Style) {
    style.flex_grow = computed
        .get("flex-grow")
        .and_then(non_negative_number)
        .unwrap_or(style.flex_grow);
    style.flex_shrink = computed
        .get("flex-shrink")
        .and_then(non_negative_number)
        .unwrap_or(style.flex_shrink);
    style.flex_basis = computed
        .get("flex-basis")
        .and_then(dimension)
        .unwrap_or(style.flex_basis);
}

fn non_negative_number(value: &str) -> Option<f32> {
    number(value).filter(|number| number.is_finite() && *number >= 0.0)
}
