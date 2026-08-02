//! CSS border shorthand resolution for GPU display-list commands.

use crate::{color, style::Computed};

use super::{
    SCALE,
    layout::{first_number, number},
};

#[derive(Clone, Copy, Eq, PartialEq)]
enum BorderStyle {
    None,
    Solid,
    Dotted,
    Dashed,
    Outset,
    Inset,
    Groove,
    Ridge,
    Double,
}

impl BorderStyle {
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("none" | "hidden") => Self::None,
            Some("dotted") => Self::Dotted,
            Some("dashed") => Self::Dashed,
            Some("outset") => Self::Outset,
            Some("inset") => Self::Inset,
            Some("groove") => Self::Groove,
            Some("ridge") => Self::Ridge,
            Some("double") => Self::Double,
            _ => Self::Solid,
        }
    }

    fn protocol(self) -> display_proto::BorderStyle {
        match self {
            Self::None => display_proto::BorderStyle::None,
            Self::Solid => display_proto::BorderStyle::Solid,
            Self::Dotted => display_proto::BorderStyle::Dotted,
            Self::Dashed => display_proto::BorderStyle::Dashed,
            Self::Outset => display_proto::BorderStyle::Outset,
            Self::Inset => display_proto::BorderStyle::Inset,
            Self::Groove => display_proto::BorderStyle::Groove,
            Self::Ridge => display_proto::BorderStyle::Ridge,
            Self::Double => display_proto::BorderStyle::Double,
        }
    }
}

pub(super) fn gpu_border(
    computed: &Computed,
) -> ([f32; 4], [u32; 4], [display_proto::BorderStyle; 4]) {
    let uniform_width = computed
        .get("border-width")
        .and_then(number)
        .or_else(|| computed.get("border").and_then(first_number))
        .unwrap_or(0.0);
    let uniform_color = computed
        .get("border-color")
        .and_then(color::parse)
        .or_else(|| computed.get("border").and_then(last_color));
    let uniform_style = computed
        .get("border-style")
        .map(|value| BorderStyle::parse(Some(value)))
        .or_else(|| computed.get("border").and_then(border_style))
        .unwrap_or(BorderStyle::Solid);
    let mut widths = [0.0; 4];
    let mut colors = [0; 4];
    let mut styles = [display_proto::BorderStyle::None; 4];
    for (index, side) in ["top", "right", "bottom", "left"].iter().enumerate() {
        let shorthand = computed.get(&format!("border-{side}"));
        let width = computed
            .get(&format!("border-{side}-width"))
            .and_then(number)
            .or_else(|| shorthand.and_then(first_number))
            .unwrap_or(uniform_width);
        let Some(side_color) = computed
            .get(&format!("border-{side}-color"))
            .and_then(color::parse)
            .or_else(|| shorthand.and_then(last_color))
            .or(uniform_color)
        else {
            continue;
        };
        let style = computed
            .get(&format!("border-{side}-style"))
            .map(|value| BorderStyle::parse(Some(value)))
            .or_else(|| shorthand.and_then(border_style))
            .unwrap_or(uniform_style);
        if width > 0.0 && style != BorderStyle::None {
            widths[index] = width * SCALE;
            colors[index] = side_color;
            styles[index] = style.protocol();
        }
    }
    (widths, colors, styles)
}

fn last_color(value: &str) -> Option<u32> {
    value.split_whitespace().rev().find_map(color::parse)
}

fn border_style(value: &str) -> Option<BorderStyle> {
    value.split_whitespace().find_map(|token| match token {
        "none" | "hidden" => Some(BorderStyle::None),
        "dotted" => Some(BorderStyle::Dotted),
        "dashed" => Some(BorderStyle::Dashed),
        "outset" => Some(BorderStyle::Outset),
        "inset" => Some(BorderStyle::Inset),
        "groove" => Some(BorderStyle::Groove),
        "ridge" => Some(BorderStyle::Ridge),
        "double" => Some(BorderStyle::Double),
        "solid" => Some(BorderStyle::Solid),
        _ => None,
    })
}
