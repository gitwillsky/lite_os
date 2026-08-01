use std::collections::BTreeMap;

use super::{Keyframe, Keyframes, Timeline, Timing, sample_value};

#[test]
fn transform_translation_interpolates_in_css_pixels() {
    assert_eq!(
        sample_value("transform", "translateX(-132px)", "translateX(476px)", 0.5),
        "translate(172px, 0px)"
    );
}

#[test]
fn display_none_switches_only_at_the_effect_boundary() {
    assert_eq!(sample_value("display", "block", "none", 0.999), "block");
    assert_eq!(sample_value("display", "block", "none", 1.0), "none");
    assert_eq!(sample_value("display", "none", "block", 0.0), "block");
}

#[test]
fn ease_out_preserves_endpoints_and_advances_faster_than_linear() {
    let timing = Timing::parse("ease-out").expect("known timing");
    assert_eq!(timing.sample(0.0), 0.0);
    assert_eq!(timing.sample(1.0), 1.0);
    assert!(timing.sample(0.5) > 0.5);
}

#[test]
fn finite_animation_holds_its_terminal_frame_and_goes_idle() {
    let keyframes = BTreeMap::from([(
        "exit".to_owned(),
        Keyframes {
            frames: vec![
                Keyframe {
                    offset: 0.0,
                    declarations: vec![("opacity".to_owned(), "1".to_owned())],
                },
                Keyframe {
                    offset: 1.0,
                    declarations: vec![("opacity".to_owned(), "0".to_owned())],
                },
            ],
        },
    )]);
    let mut timeline = Timeline::new();
    timeline.now_ms = 0.0;
    let mut values = BTreeMap::from([
        (
            "animation".to_owned(),
            "exit 890ms linear 1 both".to_owned(),
        ),
        ("opacity".to_owned(), "0".to_owned()),
    ]);
    timeline.apply_animation(1, &mut values, &keyframes);
    assert!(timeline.active);
    assert_eq!(values.get("opacity").map(String::as_str), Some("1"));

    timeline.active = false;
    timeline.now_ms = 900.0;
    timeline.apply_animation(1, &mut values, &keyframes);
    assert!(!timeline.active);
    assert_eq!(values.get("opacity").map(String::as_str), Some("0"));
}

#[test]
fn transition_scoped_to_a_state_rule_starts_from_the_initial_value() {
    let mut timeline = Timeline::new();
    // Not hovered: neither a `transition` declaration nor a transform value.
    timeline.now_ms = 0.0;
    let mut idle = BTreeMap::new();
    timeline.apply_transitions(7, &mut idle);

    // Hover enters: the transition is declared only by the state rule and must
    // start from the initial value (`none`).
    timeline.now_ms = 20.0;
    let mut hovered = BTreeMap::from([
        ("transition".to_owned(), "transform 180ms linear".to_owned()),
        ("transform".to_owned(), "translateY(-4px)".to_owned()),
    ]);
    timeline.apply_transitions(7, &mut hovered);
    assert!(timeline.active);
    assert_eq!(
        hovered.get("transform").map(String::as_str),
        Some("translate(0px, 0px)")
    );

    // The effect completes at the target and goes idle.
    timeline.active = false;
    timeline.now_ms = 300.0;
    let mut settled = BTreeMap::from([
        ("transition".to_owned(), "transform 180ms linear".to_owned()),
        ("transform".to_owned(), "translateY(-4px)".to_owned()),
    ]);
    timeline.apply_transitions(7, &mut settled);
    assert_eq!(
        settled.get("transform").map(String::as_str),
        Some("translateY(-4px)")
    );
    assert!(!timeline.active);
}

#[test]
fn transition_restarts_after_the_declaration_disappears_and_returns() {
    let mut timeline = Timeline::new();
    timeline.now_ms = 0.0;
    let mut hovered = BTreeMap::from([
        ("transition".to_owned(), "transform 180ms linear".to_owned()),
        ("transform".to_owned(), "translateY(-4px)".to_owned()),
    ]);
    timeline.apply_transitions(9, &mut hovered);
    assert!(timeline.active);

    // Hover leaves: the declaration disappears, the running transition is
    // cancelled (snap back), but the node state survives with its baseline
    // refreshed to the current value.
    timeline.active = false;
    timeline.now_ms = 50.0;
    let mut idle = BTreeMap::new();
    timeline.apply_transitions(9, &mut idle);
    assert!(!timeline.active);

    // Hovering again must start the transition from the initial value again.
    timeline.now_ms = 100.0;
    let mut rehovered = BTreeMap::from([
        ("transition".to_owned(), "transform 180ms linear".to_owned()),
        ("transform".to_owned(), "translateY(-4px)".to_owned()),
    ]);
    timeline.apply_transitions(9, &mut rehovered);
    assert!(timeline.active);
    assert_eq!(
        rehovered.get("transform").map(String::as_str),
        Some("translate(0px, 0px)")
    );
}

#[test]
fn identity_transform_does_not_start_a_noop_transition() {
    let mut timeline = Timeline::new();
    timeline.now_ms = 0.0;
    let mut values = BTreeMap::from([
        ("transition".to_owned(), "transform 180ms linear".to_owned()),
        ("transform".to_owned(), "translateY(0)".to_owned()),
    ]);
    timeline.apply_transitions(11, &mut values);
    assert!(!timeline.active);
    assert_eq!(
        values.get("transform").map(String::as_str),
        Some("translateY(0)")
    );
}

#[test]
fn transition_starts_from_the_presented_value_and_stops_at_the_target() {
    let mut timeline = Timeline::new();
    timeline.now_ms = 0.0;
    let mut initial = BTreeMap::from([
        ("transition".to_owned(), "opacity 200ms linear".to_owned()),
        ("opacity".to_owned(), "0".to_owned()),
    ]);
    timeline.apply_transitions(3, &mut initial);

    timeline.now_ms = 100.0;
    let mut changed = BTreeMap::from([
        ("transition".to_owned(), "opacity 200ms linear".to_owned()),
        ("opacity".to_owned(), "1".to_owned()),
    ]);
    timeline.apply_transitions(3, &mut changed);
    assert_eq!(changed.get("opacity").map(String::as_str), Some("0"));

    timeline.active = false;
    timeline.now_ms = 300.0;
    let mut finished = BTreeMap::from([
        ("transition".to_owned(), "opacity 200ms linear".to_owned()),
        ("opacity".to_owned(), "1".to_owned()),
    ]);
    timeline.apply_transitions(3, &mut finished);
    assert_eq!(finished.get("opacity").map(String::as_str), Some("1"));
    assert!(!timeline.active);
}
