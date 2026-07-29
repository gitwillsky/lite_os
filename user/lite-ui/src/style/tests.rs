use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use super::{PseudoState, Sheet, Timeline};
use crate::tree::Node;

#[test]
fn box_longhands_follow_source_order_and_four_side_expansion() {
    let sheet = Sheet::parse(
        ".box {
                margin-top: 1px;
                margin: 2px 3px;
                margin-top: 4px;
                padding: 5px;
                padding-bottom: 6px;
                border-top: 1px solid #111111;
                border-top-width: 2px;
                border-top-color: #222222;
            }",
    )
    .expect("standard box declarations parse");
    let node = Node {
        id: 1,
        kind: "div".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String("box".to_owned()))]),
        text: String::new(),
        children: Vec::new(),
    };

    let computed = sheet.compute(&node, &[]);

    assert_eq!(computed.get("margin-top"), Some("4px"));
    assert_eq!(computed.get("margin-right"), Some("3px"));
    assert_eq!(computed.get("margin-bottom"), Some("2px"));
    assert_eq!(computed.get("margin-left"), Some("3px"));
    assert_eq!(computed.get("padding-top"), Some("5px"));
    assert_eq!(computed.get("padding-bottom"), Some("6px"));
    assert_eq!(computed.get("border-top-width"), Some("2px"));
    assert_eq!(computed.get("border-top-color"), Some("#222222"));
}

#[test]
fn later_border_shorthand_resets_earlier_side_longhands() {
    let sheet = Sheet::parse(
        ".box {
                border-top-width: 7px;
                border-top-color: #111111;
                border: 2px solid #abcdef;
            }",
    )
    .expect("standard border declarations parse");
    let node = Node {
        id: 1,
        kind: "div".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String("box".to_owned()))]),
        text: String::new(),
        children: Vec::new(),
    };

    let computed = sheet.compute(&node, &[]);

    for side in ["top", "right", "bottom", "left"] {
        assert_eq!(computed.get(&format!("border-{side}-width")), Some("2px"));
        assert_eq!(
            computed.get(&format!("border-{side}-color")),
            Some("#abcdef")
        );
        assert_eq!(computed.get(&format!("border-{side}-style")), Some("solid"));
    }
}

#[test]
fn border_style_expands_in_standard_edge_order() {
    let sheet = Sheet::parse(
        ".box {
                border-style: dotted dashed solid none;
            }",
    )
    .expect("border styles parse");
    let node = Node {
        id: 1,
        kind: "div".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String("box".to_owned()))]),
        text: String::new(),
        children: Vec::new(),
    };

    let computed = sheet.compute(&node, &[]);

    assert_eq!(computed.get("border-top-style"), Some("dotted"));
    assert_eq!(computed.get("border-right-style"), Some("dashed"));
    assert_eq!(computed.get("border-bottom-style"), Some("solid"));
    assert_eq!(computed.get("border-left-style"), Some("none"));
}

#[test]
fn background_shorthand_expands_color_image_and_tiling_longhands() {
    let sheet = Sheet::parse(
        ".box {
                background: url(\"assets/bg.png\") no-repeat center / cover;
            }",
    )
    .expect("background shorthand parses");
    let node = Node {
        id: 1,
        kind: "div".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String("box".to_owned()))]),
        text: String::new(),
        children: Vec::new(),
    };

    let computed = sheet.compute(&node, &[]);

    assert_eq!(
        computed.get("background-image"),
        Some("url(\"assets/bg.png\")")
    );
    assert_eq!(computed.get("background-color"), Some("transparent"));
    assert_eq!(computed.get("background-repeat"), Some("no-repeat"));
    assert_eq!(computed.get("background-position"), Some("center"));
    assert_eq!(computed.get("background-size"), Some("cover"));
}

#[test]
fn background_shorthand_mixes_color_gradient_and_repeat() {
    let sheet = Sheet::parse(
        ".box {
                background: repeat-x linear-gradient(90deg, #000000, #ffffff) #0a246a;
            }",
    )
    .expect("mixed background shorthand parses");
    let node = Node {
        id: 1,
        kind: "div".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String("box".to_owned()))]),
        text: String::new(),
        children: Vec::new(),
    };

    let computed = sheet.compute(&node, &[]);

    assert_eq!(computed.get("background-color"), Some("#0a246a"));
    assert_eq!(
        computed.get("background-image"),
        Some("linear-gradient(90deg, #000000, #ffffff)")
    );
    assert_eq!(computed.get("background-repeat"), Some("repeat-x"));
    // Tiling longhands absent from the shorthand stay untouched.
    assert_eq!(computed.get("background-position"), None);
    assert_eq!(computed.get("background-size"), None);
}

#[test]
fn later_background_shorthand_resets_earlier_image_longhand() {
    let sheet = Sheet::parse(
        ".box {
                background-image: url(assets/bg.png);
                background: #d4d0c8;
            }",
    )
    .expect("background reset parses");
    let node = Node {
        id: 1,
        kind: "div".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String("box".to_owned()))]),
        text: String::new(),
        children: Vec::new(),
    };

    let computed = sheet.compute(&node, &[]);

    assert_eq!(computed.get("background-color"), Some("#d4d0c8"));
    assert_eq!(computed.get("background-image"), Some("none"));
}

#[test]
fn color_function_stays_one_token_during_edge_expansion() {
    let sheet = Sheet::parse(
        ".box {
                border-color: rgba(10, 20, 30, 0.5);
            }",
    )
    .expect("functional color parses");
    let node = Node {
        id: 1,
        kind: "div".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String("box".to_owned()))]),
        text: String::new(),
        children: Vec::new(),
    };

    let computed = sheet.compute(&node, &[]);

    for side in ["top", "right", "bottom", "left"] {
        assert_eq!(
            computed.get(&format!("border-{side}-color")),
            Some("rgba(10, 20, 30, 0.5)")
        );
    }
}

#[test]
fn named_color_is_extracted_from_border_shorthand() {
    let sheet = Sheet::parse(
        ".box {
                border: 1px solid teal;
            }",
    )
    .expect("named color border parses");
    let node = Node {
        id: 1,
        kind: "div".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String("box".to_owned()))]),
        text: String::new(),
        children: Vec::new(),
    };

    let computed = sheet.compute(&node, &[]);

    for side in ["top", "right", "bottom", "left"] {
        assert_eq!(computed.get(&format!("border-{side}-color")), Some("teal"));
    }
}

#[test]
fn accent_color_reaches_standard_form_controls() {
    let sheet = Sheet::parse(".slider { accent-color: #35c8ff; }")
        .expect("accent color declaration parses");
    let node = Node {
        id: 1,
        kind: "input".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String("slider".to_owned()))]),
        text: String::new(),
        children: Vec::new(),
    };

    assert_eq!(
        sheet.compute(&node, &[]).get("accent-color"),
        Some("#35c8ff")
    );
}

#[test]
fn motion_media_and_keyframes_override_the_base_cascade() {
    let sheet = Sheet::parse(
        ".splash { opacity: 0; pointer-events: none; }
            @keyframes reveal {
                from { opacity: 1; pointer-events: auto; }
                to { opacity: 0; pointer-events: none; }
            }
            @media (prefers-reduced-motion: no-preference) {
                .splash { animation: reveal 1s linear 1 both; }
            }",
    )
    .expect("standard motion stylesheet parses");
    let node = Node {
        id: 7,
        kind: "div".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String("splash".to_owned()))]),
        text: String::new(),
        children: Vec::new(),
    };
    let mut timeline = Timeline::new();
    timeline.begin_frame();

    let computed = sheet.compute_at(&node, &[], None, &PseudoState::default(), &mut timeline);

    assert_eq!(computed.get("pointer-events"), Some("auto"));
    assert!(
        computed
            .get("opacity")
            .is_some_and(|value| { value.parse::<f32>().is_ok_and(|opacity| opacity > 0.99) })
    );
    assert!(timeline.active());
}

#[test]
fn custom_properties_cascade_inherit_and_resolve_nested_fallbacks() {
    let sheet = Sheet::parse(
        ".theme {
                --accent: #35c8ff;
                --surface: rgba(8, 17, 34, 0.88);
            }
            .panel {
                color: var(--missing, var(--accent));
                background-color: var(--surface);
                border-color: var(--accent);
            }",
    )
    .expect("custom properties parse");
    let parent = Node {
        id: 1,
        kind: "div".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String("theme".to_owned()))]),
        text: String::new(),
        children: Vec::new(),
    };
    let child = Node {
        id: 2,
        kind: "div".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String("panel".to_owned()))]),
        text: String::new(),
        children: Vec::new(),
    };

    let pseudo = PseudoState::default();
    let parent_style = sheet.cascade(&parent, &[], None, &pseudo);
    let child_style = sheet.cascade(&child, &[&parent], Some(&parent_style), &pseudo);

    assert_eq!(child_style.get("color"), Some("#35c8ff"));
    assert_eq!(
        child_style.get("background-color"),
        Some("rgba(8, 17, 34, 0.88)")
    );
    assert_eq!(child_style.get("border-color"), Some("#35c8ff"));
}

#[test]
fn invalid_variable_removes_winning_declaration_instead_of_reviving_old_value() {
    let sheet = Sheet::parse(
        ".panel {
                margin: 4px;
                margin: var(--cyclic);
                color: #ffffff;
                color: var(--absent);
                --cyclic: var(--cyclic);
            }",
    )
    .expect("invalid custom property syntax still parses");
    let node = Node {
        id: 1,
        kind: "div".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String("panel".to_owned()))]),
        text: String::new(),
        children: Vec::new(),
    };

    let computed = sheet.compute(&node, &[]);

    assert_eq!(computed.get("color"), None);
    assert_eq!(computed.get("margin"), None);
    for side in ["top", "right", "bottom", "left"] {
        assert_eq!(computed.get(&format!("margin-{side}")), None);
    }
}

#[test]
fn standard_pseudo_classes_match_dynamic_state_and_hovered_ancestors() {
    let sheet = Sheet::parse(
        ".button { color: #ffffff; }
            .button:hover { color: #35c8ff; }
            .button:active { transform: scale(0.96); }
            .button:focus { border-color: #35c8ff; }
            .button:disabled { opacity: 0.4; }",
    )
    .expect("standard pseudo-classes parse");
    let button = Node {
        id: 10,
        kind: "button".to_owned(),
        props: BTreeMap::from([
            ("className".to_owned(), Value::String("button".to_owned())),
            ("disabled".to_owned(), Value::Bool(true)),
        ]),
        text: String::new(),
        children: Vec::new(),
    };
    let parents = HashMap::from([(10, None), (11, Some(10))]);
    let pseudo = PseudoState::from_targets(&parents, Some(11), Some(11), Some(10));

    let computed = sheet.cascade(&button, &[], None, &pseudo);

    assert_eq!(computed.get("color"), Some("#35c8ff"));
    assert_eq!(computed.get("transform"), Some("scale(0.96)"));
    assert_eq!(computed.get("border-color"), Some("#35c8ff"));
    assert_eq!(computed.get("opacity"), Some("0.4"));
}

#[test]
fn css_wide_keywords_resolve_against_parent_computed_values() {
    let sheet = Sheet::parse(
        ".parent { color: #f3f7ff; font-size: 16px; padding: 9px; }
            .child { color: inherit; font-size: unset; padding: initial; }",
    )
    .expect("CSS-wide keywords parse");
    let parent = Node {
        id: 1,
        kind: "div".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String("parent".to_owned()))]),
        text: String::new(),
        children: Vec::new(),
    };
    let child = Node {
        id: 2,
        kind: "button".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String("child".to_owned()))]),
        text: String::new(),
        children: Vec::new(),
    };
    let pseudo = PseudoState::default();
    let parent_style = sheet.cascade(&parent, &[], None, &pseudo);
    let child_style = sheet.cascade(&child, &[&parent], Some(&parent_style), &pseudo);

    assert_eq!(child_style.get("color"), Some("#f3f7ff"));
    assert_eq!(child_style.get("font-size"), Some("16px"));
    assert_eq!(child_style.get("padding"), None);
    assert_eq!(child_style.get("padding-left"), None);
}

#[test]
fn nth_child_matches_standard_an_plus_b_and_ignores_text_nodes() {
    let sheet = Sheet::parse(
        ".item:nth-child(2n + 1) { color: #35c8ff; }
            .item { color: #ffffff; }
            .item:nth-child(-n+2) { opacity: 0.5; }",
    )
    .expect("standard :nth-child() expressions parse");
    let first = Node {
        id: 2,
        kind: "span".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String("item".to_owned()))]),
        text: String::new(),
        children: Vec::new(),
    };
    let second = Node {
        id: 4,
        kind: "span".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String("item".to_owned()))]),
        text: String::new(),
        children: Vec::new(),
    };
    let parent = Node {
        id: 1,
        kind: "div".to_owned(),
        props: BTreeMap::new(),
        text: String::new(),
        children: vec![
            first.clone(),
            Node {
                id: 3,
                kind: "#text".to_owned(),
                props: BTreeMap::new(),
                text: "separator".to_owned(),
                children: Vec::new(),
            },
            second.clone(),
        ],
    };

    let first_style = sheet.compute(&first, &[&parent]);
    let second_style = sheet.compute(&second, &[&parent]);

    assert_eq!(first_style.get("color"), Some("#35c8ff"));
    assert_eq!(first_style.get("opacity"), Some("0.5"));
    assert_eq!(second_style.get("color"), Some("#ffffff"));
    assert_eq!(second_style.get("opacity"), Some("0.5"));
}

#[test]
fn selector_lists_share_declarations_without_changing_specificity() {
    let sheet = Sheet::parse(
        ".primary, .secondary { color: #35c8ff; }
            .primary:hover, button:focus { border-color: #8358ff; }",
    )
    .expect("selector lists parse");
    for class_name in ["primary", "secondary"] {
        let node = Node {
            id: 1,
            kind: "button".to_owned(),
            props: BTreeMap::from([("className".to_owned(), Value::String(class_name.to_owned()))]),
            text: String::new(),
            children: Vec::new(),
        };
        assert_eq!(sheet.compute(&node, &[]).get("color"), Some("#35c8ff"));
    }
}

#[test]
fn child_combinator_requires_the_immediate_parent() {
    let sheet = Sheet::parse(
        ".panel > span { color: #35c8ff; }
             .desktop .panel > span { opacity: 0.75; }",
    )
    .expect("child combinators parse");
    let desktop = Node {
        id: 1,
        kind: "div".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String("desktop".to_owned()))]),
        text: String::new(),
        children: Vec::new(),
    };
    let panel = Node {
        id: 2,
        kind: "div".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String("panel".to_owned()))]),
        text: String::new(),
        children: Vec::new(),
    };
    let wrapper = Node {
        id: 3,
        kind: "div".to_owned(),
        props: BTreeMap::new(),
        text: String::new(),
        children: Vec::new(),
    };
    let span = Node {
        id: 4,
        kind: "span".to_owned(),
        props: BTreeMap::new(),
        text: String::new(),
        children: Vec::new(),
    };

    let direct = sheet.compute(&span, &[&desktop, &panel]);
    assert_eq!(direct.get("color"), Some("#35c8ff"));
    assert_eq!(direct.get("opacity"), Some("0.75"));

    let nested = sheet.compute(&span, &[&desktop, &panel, &wrapper]);
    assert_eq!(nested.get("color"), None);
    assert_eq!(nested.get("opacity"), None);
}

#[test]
fn unknown_media_query_is_rejected_instead_of_silently_matching() {
    assert!(Sheet::parse("@media (width > 10px) { .box { opacity: 1; } }").is_err());
}
