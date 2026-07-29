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
