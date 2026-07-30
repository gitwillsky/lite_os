//! CSS positioned inset value lowering.

use taffy::LengthPercentageAuto;

use super::number;

pub(super) fn length_auto(value: Option<&str>) -> LengthPercentageAuto {
    match value {
        Some("auto") | None => LengthPercentageAuto::auto(),
        Some(value) if value.ends_with('%') => value
            .strip_suffix('%')
            .and_then(|value| value.trim().parse::<f32>().ok())
            .map(|value| LengthPercentageAuto::percent(value / 100.0))
            .unwrap_or(LengthPercentageAuto::auto()),
        Some(value) => number(value)
            .map(LengthPercentageAuto::length)
            .unwrap_or(LengthPercentageAuto::auto()),
    }
}

#[cfg(test)]
mod tests {
    use taffy::LengthPercentageAuto;

    use super::length_auto;

    #[test]
    fn positioned_insets_accept_standard_percentages() {
        assert_eq!(length_auto(Some("50%")), LengthPercentageAuto::percent(0.5));
        assert_eq!(
            length_auto(Some("25%")),
            LengthPercentageAuto::percent(0.25)
        );
    }
}
