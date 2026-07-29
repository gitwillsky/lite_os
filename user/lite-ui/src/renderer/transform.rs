//! CSS transform parsing shared by paint, descendant geometry and hit testing.

use crate::style::Computed;

/// Resolves the supported translation transform in logical CSS pixels.
pub(super) fn translation(computed: &Computed) -> (f32, f32) {
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
        return (css_px(inner).unwrap_or(0.0), 0.0);
    }
    if let Some(inner) = value
        .strip_prefix("translateY(")
        .and_then(|value| value.strip_suffix(')'))
    {
        return (0.0, css_px(inner).unwrap_or(0.0));
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
            .and_then(|value| css_px(value))
            .unwrap_or(0.0),
        components
            .get(1)
            .and_then(|value| css_px(value))
            .unwrap_or(0.0),
    )
}

fn css_px(value: &str) -> Option<f32> {
    value.trim().strip_suffix("px")?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::translation;
    use crate::style::Computed;

    #[test]
    fn translation_accepts_axis_and_pair_functions() {
        let mut style = Computed::default();
        style.set("transform", "translateX(12px)");
        assert_eq!(translation(&style), (12.0, 0.0));
        style.set("transform", "translate(-3px, 4px)");
        assert_eq!(translation(&style), (-3.0, 4.0));
    }
}
