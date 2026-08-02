//! Standard controlled `<input type="range">` state and input mapping.

use std::collections::BTreeMap;

use serde_json::Value;

const DEFAULT_MIN: f64 = 0.0;
const DEFAULT_MAX: f64 = 100.0;
const DEFAULT_STEP: f64 = 1.0;
const THUMB_WIDTH: f32 = 12.0;

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
        let value = normalize(
            property_number(props, "value").unwrap_or(min + (max - min) / 2.0),
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

    pub(crate) fn on_input(self) -> Option<u64> {
        self.on_input
    }
    pub(crate) fn disabled(self) -> bool {
        self.disabled
    }
    pub(crate) fn value(self) -> f64 {
        self.value
    }

    pub(crate) fn fraction(self) -> f32 {
        if self.max <= self.min {
            0.0
        } else {
            ((self.value - self.min) / (self.max - self.min)).clamp(0.0, 1.0) as f32
        }
    }

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

    pub(crate) fn stepped(self, direction: i32) -> f64 {
        normalize(
            self.value + self.step.unwrap_or(DEFAULT_STEP) * f64::from(direction),
            self.min,
            self.max,
            self.step,
        )
    }

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
    step.map_or(value, |step| {
        (min + ((value - min) / step).round() * step).clamp(min, max)
    })
}

#[cfg(test)]
mod tests {
    use super::RangeInput;
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

    fn range(properties: Value) -> RangeInput {
        let properties: BTreeMap<String, Value> = serde_json::from_value(properties).unwrap();
        RangeInput::from_props(&properties, Some(7)).unwrap()
    }

    #[test]
    fn range_clamps_snaps_and_maps_pointer() {
        let input = range(json!({"type":"range","min":0,"max":100,"step":1,"value":50}));
        assert_eq!(input.value_at(66.0, 10.0, 112.0), 50.0);
        assert_eq!(RangeInput::string_value(input.stepped(1)), "51");
    }
}
