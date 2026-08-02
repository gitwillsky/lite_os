//! CSS box-shadow parsing for GPU display-list commands.

use crate::color;

use super::{gradient::split_top_level, layout::number};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Shadow {
    pub(super) dx: f32,
    pub(super) dy: f32,
    pub(super) blur: f32,
    pub(super) spread: f32,
    pub(super) color: u32,
    pub(super) inset: bool,
}

pub(super) fn parse_box_shadows(value: &str, current_color: u32) -> Vec<Shadow> {
    parse_shadows(value, current_color, true)
}

pub(super) fn parse_text_shadows(value: &str, current_color: u32) -> Vec<Shadow> {
    parse_shadows(value, current_color, false)
}

fn parse_shadows(value: &str, current_color: u32, box_shadow: bool) -> Vec<Shadow> {
    split_top_level(value, ',')
        .into_iter()
        .filter_map(|segment| {
            let mut lengths = Vec::new();
            let mut color = None;
            let mut inset = false;
            for token in crate::style::split_css_tokens(segment.trim()) {
                if token == "inset" && box_shadow {
                    inset = true;
                } else if token.eq_ignore_ascii_case("currentcolor") {
                    color = Some(current_color);
                } else if let Some(parsed) = color::parse(token) {
                    color = Some(parsed);
                } else if let Some(length) = number(token) {
                    lengths.push(length);
                } else {
                    return None;
                }
            }
            let maximum = if box_shadow { 4 } else { 3 };
            if !(2..=maximum).contains(&lengths.len())
                || lengths.get(2).is_some_and(|blur| *blur < 0.0)
            {
                return None;
            }
            Some(Shadow {
                dx: lengths[0],
                dy: lengths[1],
                blur: lengths.get(2).copied().unwrap_or(0.0),
                spread: lengths.get(3).copied().unwrap_or(0.0),
                color: color.unwrap_or(current_color),
                inset,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_shadow_layers() {
        let shadows =
            parse_box_shadows("0 2px 8px #80000000, inset 0 0 2px #ff112233", 0xff00_0000);
        assert_eq!(shadows.len(), 2);
        assert!(!shadows[0].inset);
        assert!(shadows[1].inset);
    }

    #[test]
    fn text_shadow_defaults_to_current_color_and_rejects_spread() {
        let shadows = parse_text_shadows("1px 2px 3px", 0xff12_3456);
        assert_eq!(shadows[0].color, 0xff12_3456);
        assert!(parse_text_shadows("1px 2px 3px 4px", 0xff00_0000).is_empty());
    }
}
