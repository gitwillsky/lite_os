//! Standard horizontal `<input type="range">` state, input mapping and UA paint.

use std::collections::BTreeMap;

use linux_uapi::drm::SharedDumbBuffer;
use serde_json::Value;

use super::{PhysicalRect, SCALE, box_paint::paint_background};

const DEFAULT_MIN: f64 = 0.0;
const DEFAULT_MAX: f64 = 100.0;
const DEFAULT_STEP: f64 = 1.0;
const THUMB_WIDTH: f32 = 12.0;

/// One checked controlled range input projected from React properties.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RangeInput {
    min: f64,
    max: f64,
    step: Option<f64>,
    value: f64,
    on_input: Option<u64>,
    disabled: bool,
}

impl RangeInput {
    /// Parses standard range properties; returns `None` for non-range inputs.
    pub(crate) fn from_props(
        props: &BTreeMap<String, Value>,
        on_input: Option<u64>,
    ) -> Option<Self> {
        if props.get("type").and_then(Value::as_str) != Some("range") {
            return None;
        }
        let min = property_number(props, "min").unwrap_or(DEFAULT_MIN);
        let max = property_number(props, "max")
            .unwrap_or(DEFAULT_MAX)
            .max(min);
        let step = match props.get("step") {
            Some(Value::String(value)) if value == "any" => None,
            _ => Some(
                property_number(props, "step")
                    .filter(|value| *value > 0.0)
                    .unwrap_or(DEFAULT_STEP),
            ),
        };
        let fallback = min + (max - min) / 2.0;
        let value = normalize(
            property_number(props, "value").unwrap_or(fallback),
            min,
            max,
            step,
        );
        Some(Self {
            min,
            max,
            step,
            value,
            on_input,
            disabled: props
                .get("disabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    /// Returns the controlled input listener receiving standard string values.
    pub(crate) fn on_input(self) -> Option<u64> {
        self.on_input
    }

    /// Returns whether pointer and keyboard default actions are disabled.
    pub(crate) fn disabled(self) -> bool {
        self.disabled
    }

    /// Returns the normalized controlled value used for change suppression.
    pub(crate) fn value(self) -> f64 {
        self.value
    }

    /// Maps a surface-local pointer coordinate onto the range track.
    pub(crate) fn value_at(self, pointer_x: f32, left: f32, width: f32) -> f64 {
        let inset = THUMB_WIDTH / 2.0;
        let track_width = (width - THUMB_WIDTH).max(1.0);
        let fraction = ((pointer_x - left - inset) / track_width).clamp(0.0, 1.0);
        normalize(
            self.min + (self.max - self.min) * f64::from(fraction),
            self.min,
            self.max,
            self.step,
        )
    }

    /// Applies one keyboard arrow step in `direction` (`-1` or `1`).
    pub(crate) fn stepped(self, direction: i32) -> f64 {
        let step = self.step.unwrap_or(DEFAULT_STEP);
        normalize(
            self.value + step * f64::from(direction),
            self.min,
            self.max,
            self.step,
        )
    }

    /// Serializes a finite range value using HTML input's string-valued event shape.
    pub(crate) fn string_value(value: f64) -> String {
        let mut output = format!("{value:.6}");
        while output.ends_with('0') {
            output.pop();
        }
        if output.ends_with('.') {
            output.pop();
        }
        if output == "-0" {
            output = "0".to_owned();
        }
        output
    }

    fn fraction(self) -> f32 {
        if self.max <= self.min {
            0.0
        } else {
            ((self.value - self.min) / (self.max - self.min)).clamp(0.0, 1.0) as f32
        }
    }
}

/// Paints the fixed LiteOS horizontal range user-agent appearance.
pub(super) fn paint_range(
    pixels: &mut SharedDumbBuffer,
    bounds: PhysicalRect,
    clip: Option<PhysicalRect>,
    range: RangeInput,
    focused: bool,
) {
    if bounds.x2 <= bounds.x1 || bounds.y2 <= bounds.y1 {
        return;
    }
    let unit = SCALE.round() as usize;
    let inset = (THUMB_WIDTH / 2.0 * SCALE).round() as usize;
    let track_left = (bounds.x1 + inset).min(bounds.x2);
    let track_right = bounds.x2.saturating_sub(inset).max(track_left);
    let center_y = bounds.y1 + (bounds.y2 - bounds.y1) / 2;
    let track_half = (2.0 * SCALE).round() as usize;
    let track = PhysicalRect {
        x1: track_left,
        y1: center_y.saturating_sub(track_half),
        x2: track_right,
        y2: (center_y + track_half).min(bounds.y2),
    };
    fill(pixels, track, clip, "#7f9db9");
    let inner = PhysicalRect {
        x1: (track.x1 + unit).min(track.x2),
        y1: (track.y1 + unit).min(track.y2),
        x2: track.x2.saturating_sub(unit).max(track.x1),
        y2: track.y2.saturating_sub(unit).max(track.y1),
    };
    fill(pixels, inner, clip, "#ffffff");

    let travel = track_right.saturating_sub(track_left);
    let thumb_center = track_left
        + (travel as f32 * range.fraction())
            .round()
            .clamp(0.0, travel as f32) as usize;
    let progress = PhysicalRect {
        x1: inner.x1,
        y1: inner.y1,
        x2: thumb_center.clamp(inner.x1, inner.x2),
        y2: inner.y2,
    };
    fill(
        pixels,
        progress,
        clip,
        if range.disabled { "#aca899" } else { "#6ba92f" },
    );

    let thumb_half_width = (THUMB_WIDTH / 2.0 * SCALE).round() as usize;
    let thumb_half_height = (9.0 * SCALE).round() as usize;
    let thumb = PhysicalRect {
        x1: thumb_center.saturating_sub(thumb_half_width),
        y1: center_y.saturating_sub(thumb_half_height),
        x2: (thumb_center + thumb_half_width).min(bounds.x2),
        y2: (center_y + thumb_half_height).min(bounds.y2),
    };
    fill(
        pixels,
        thumb,
        clip,
        if focused { "#003c74" } else { "#7f9db9" },
    );
    let highlight = inset_rect(thumb, unit);
    fill(pixels, highlight, clip, "#ffffff");
    let face = PhysicalRect {
        x1: (highlight.x1 + unit).min(highlight.x2),
        y1: (highlight.y1 + unit).min(highlight.y2),
        x2: highlight.x2,
        y2: highlight.y2,
    };
    fill(
        pixels,
        face,
        clip,
        if range.disabled { "#d4d0c8" } else { "#ece9d8" },
    );
}

fn property_number(props: &BTreeMap<String, Value>, name: &str) -> Option<f64> {
    let value = props.get(name)?;
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|value| value.is_finite())
}

fn normalize(value: f64, min: f64, max: f64, step: Option<f64>) -> f64 {
    let value = value.clamp(min, max);
    let Some(step) = step else {
        return value;
    };
    (min + ((value - min) / step).round() * step).clamp(min, max)
}

fn inset_rect(rect: PhysicalRect, amount: usize) -> PhysicalRect {
    PhysicalRect {
        x1: (rect.x1 + amount).min(rect.x2),
        y1: (rect.y1 + amount).min(rect.y2),
        x2: rect.x2.saturating_sub(amount).max(rect.x1),
        y2: rect.y2.saturating_sub(amount).max(rect.y1),
    }
}

fn fill(
    pixels: &mut SharedDumbBuffer,
    rect: PhysicalRect,
    clip: Option<PhysicalRect>,
    color: &str,
) {
    let rect = clip.map_or(rect, |clip| rect.intersect(clip));
    if !rect.is_empty() {
        paint_background(pixels, rect, color, [0.0; 4]);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::{Value, json};

    use super::RangeInput;

    fn range(properties: Value) -> RangeInput {
        let properties: BTreeMap<String, Value> =
            serde_json::from_value(properties).expect("range properties");
        RangeInput::from_props(&properties, Some(7)).expect("range input")
    }

    #[test]
    fn defaults_clamp_and_snap_like_a_controlled_html_range() {
        let default = range(json!({"type": "range"}));
        assert_eq!(RangeInput::string_value(default.value), "50");

        let snapped = range(json!({
            "type": "range",
            "min": 0,
            "max": 10,
            "step": 0.25,
            "value": 11
        }));
        assert_eq!(RangeInput::string_value(snapped.value), "10");
        assert_eq!(RangeInput::string_value(snapped.stepped(-1)), "9.75");
    }

    #[test]
    fn pointer_mapping_reserves_half_a_thumb_at_both_track_edges() {
        let input = range(json!({
            "type": "range",
            "min": 0,
            "max": 100,
            "step": 1,
            "value": 50
        }));
        assert_eq!(input.value_at(10.0, 10.0, 112.0), 0.0);
        assert_eq!(input.value_at(66.0, 10.0, 112.0), 50.0);
        assert_eq!(input.value_at(122.0, 10.0, 112.0), 100.0);
    }

    #[test]
    fn non_range_inputs_do_not_acquire_range_default_actions() {
        let properties: BTreeMap<String, Value> =
            serde_json::from_value(json!({"type": "text", "value": "50"}))
                .expect("text properties");
        assert!(RangeInput::from_props(&properties, Some(7)).is_none());
    }
}
