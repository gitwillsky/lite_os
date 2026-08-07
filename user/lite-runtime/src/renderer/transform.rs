//! CSS transform parsing shared by paint, descendant geometry and hit testing.

use crate::style::Computed;

/// Resolves a supported translation against the node's border box.
///
/// # Arguments
///
/// - `computed`: Cascaded style containing the optional `transform` value.
/// - `size`: Logical border-box width and height used by percentage operands.
///
/// # Returns
///
/// The x/y translation in logical CSS pixels. Invalid or unsupported values
/// resolve to the identity transform.
pub(super) fn translation(computed: &Computed, size: (f32, f32)) -> (f32, f32) {
    let Some(value) = computed.get("transform").map(str::trim) else {
        return (0.0, 0.0);
    };
    if value == "none" {
        return (0.0, 0.0);
    }
    if let Some(inner) = value
        .strip_prefix("translateX(")
        .and_then(|value| value.strip_suffix(')'))
    {
        return (css_distance(inner, size.0).unwrap_or(0.0), 0.0);
    }
    if let Some(inner) = value
        .strip_prefix("translateY(")
        .and_then(|value| value.strip_suffix(')'))
    {
        return (0.0, css_distance(inner, size.1).unwrap_or(0.0));
    }
    let Some(inner) = value
        .strip_prefix("translate(")
        .and_then(|value| value.strip_suffix(')'))
    else {
        return (0.0, 0.0);
    };
    let components: Vec<&str> = inner
        .split([',', ' '])
        .filter(|part| !part.trim().is_empty())
        .collect();
    (
        components
            .first()
            .and_then(|value| css_distance(value, size.0))
            .unwrap_or(0.0),
        components
            .get(1)
            .and_then(|value| css_distance(value, size.1))
            .unwrap_or(0.0),
    )
}

fn css_distance(value: &str, reference: f32) -> Option<f32> {
    let value = value.trim();
    if let Some(percent) = value.strip_suffix('%') {
        return Some(percent.trim().parse::<f32>().ok()? * reference / 100.0);
    }
    value.strip_suffix("px")?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::translation;
    use crate::style::Computed;

    #[test]
    fn translation_accepts_axis_and_pair_functions() {
        let mut style = Computed::default();
        style.set("transform", "translateX(12px)");
        assert_eq!(translation(&style, (200.0, 80.0)), (12.0, 0.0));
        style.set("transform", "translate(-3px, 4px)");
        assert_eq!(translation(&style, (200.0, 80.0)), (-3.0, 4.0));
        style.set("transform", "translate(-50%, 25%)");
        assert_eq!(translation(&style, (200.0, 80.0)), (-100.0, 20.0));
    }
}
