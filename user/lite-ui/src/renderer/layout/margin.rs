//! CSS margin value lowering, including the standard `auto` keyword.

use taffy::prelude::{LengthPercentageAuto, Rect as TaffyRect};

use super::number;

pub(super) fn edges(value: &str) -> Option<TaffyRect<LengthPercentageAuto>> {
    let values = value
        .split_whitespace()
        .map(single)
        .collect::<Option<Vec<_>>>()?;
    let [top, right, bottom, left] = match values.as_slice() {
        [all] => [*all; 4],
        [vertical, horizontal] => [*vertical, *horizontal, *vertical, *horizontal],
        [top, horizontal, bottom] => [*top, *horizontal, *bottom, *horizontal],
        [top, right, bottom, left] => [*top, *right, *bottom, *left],
        _ => return None,
    };
    Some(TaffyRect {
        top,
        right,
        bottom,
        left,
    })
}

pub(super) fn single(value: &str) -> Option<LengthPercentageAuto> {
    let value = value.trim();
    if value == "auto" {
        Some(LengthPercentageAuto::auto())
    } else if let Some(percent) = value.strip_suffix('%') {
        Some(LengthPercentageAuto::percent(
            percent.trim().parse::<f32>().ok()? / 100.0,
        ))
    } else {
        Some(LengthPercentageAuto::length(number(value)?))
    }
}

#[cfg(test)]
mod tests {
    use taffy::prelude::LengthPercentageAuto;

    #[test]
    fn expands_auto_and_percentage_in_standard_edge_order() {
        let edges = super::edges("2px auto 4px 10%").expect("valid margin");
        assert_eq!(edges.top, LengthPercentageAuto::length(2.0));
        assert_eq!(edges.right, LengthPercentageAuto::auto());
        assert_eq!(edges.bottom, LengthPercentageAuto::length(4.0));
        assert_eq!(edges.left, LengthPercentageAuto::percent(0.1));
    }
}
