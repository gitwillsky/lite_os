use liteui_core::{Mutation, NodeId, Rect, Style};

use super::{Size, Window};

pub(super) const TITLE_HEIGHT: usize = 30;
pub(super) const TASKBAR_HEIGHT: usize = 36;
pub(super) const SHADOW: usize = 8;

const ACCENTS: [u32; 3] = [0x00000080, 0x001082c4, 0x006b2f8a];
const SHADOW_NODE: NodeId = NodeId::new(2, 1);
const WINDOW_NODE: NodeId = NodeId::new(3, 1);
const TITLE_NODE: NodeId = NodeId::new(4, 1);
const CONTENT_NODE: NodeId = NodeId::new(5, 1);
const SIDEBAR_NODE: NodeId = NodeId::new(6, 1);
const CARD_NODE: NodeId = NodeId::new(7, 1);
const TASKBAR_NODE: NodeId = NodeId::new(8, 1);
const START_NODE: NodeId = NodeId::new(9, 1);

pub(super) fn root_style(viewport: Size) -> Style {
    flat_style(ui_rect(0, 0, viewport.width, viewport.height), 0x00008080)
}

pub(super) fn create_nodes(
    viewport: Size,
    window: Window,
    focused: bool,
    accent: usize,
) -> [Mutation; 8] {
    let sidebar_width = (window.width / 4).max(72);
    let content_height = window.height.saturating_sub(TITLE_HEIGHT + 4);
    [
        Mutation::Create {
            id: SHADOW_NODE,
            parent: NodeId::ROOT,
            style: shadow_style(window),
        },
        Mutation::Create {
            id: WINDOW_NODE,
            parent: NodeId::ROOT,
            style: window_style(window, focused, accent),
        },
        Mutation::Create {
            id: TITLE_NODE,
            parent: WINDOW_NODE,
            style: title_style(window.width, focused, accent),
        },
        Mutation::Create {
            id: CONTENT_NODE,
            parent: WINDOW_NODE,
            style: flat_style(
                ui_rect(
                    3,
                    TITLE_HEIGHT + 1,
                    window.width.saturating_sub(6),
                    content_height,
                ),
                0x00ffffff,
            ),
        },
        Mutation::Create {
            id: SIDEBAR_NODE,
            parent: CONTENT_NODE,
            style: flat_style(ui_rect(0, 0, sidebar_width, content_height), 0x00d4d0c8),
        },
        Mutation::Create {
            id: CARD_NODE,
            parent: CONTENT_NODE,
            style: bordered_style(
                ui_rect(
                    sidebar_width + 20,
                    28,
                    window.width.saturating_sub(sidebar_width + 44),
                    window.height.saturating_sub(TITLE_HEIGHT + 56),
                ),
                0x00f4f4f4,
                0x00808080,
                1,
            ),
        },
        Mutation::Create {
            id: TASKBAR_NODE,
            parent: NodeId::ROOT,
            style: bordered_style(
                ui_rect(
                    0,
                    viewport.height.saturating_sub(TASKBAR_HEIGHT),
                    viewport.width,
                    TASKBAR_HEIGHT,
                ),
                0x00c0c0c0,
                0x00ffffff,
                1,
            ),
        },
        Mutation::Create {
            id: START_NODE,
            parent: TASKBAR_NODE,
            style: bordered_style(ui_rect(4, 4, 72, 28), 0x00c0c0c0, 0x00ffffff, 2),
        },
    ]
}

pub(super) fn geometry_mutations(window: Window, focused: bool, accent: usize) -> [Mutation; 2] {
    [
        Mutation::SetStyle {
            id: SHADOW_NODE,
            style: shadow_style(window),
        },
        Mutation::SetStyle {
            id: WINDOW_NODE,
            style: window_style(window, focused, accent),
        },
    ]
}

pub(super) fn chrome_mutations(window: Window, focused: bool, accent: usize) -> [Mutation; 2] {
    [
        Mutation::SetStyle {
            id: WINDOW_NODE,
            style: window_style(window, focused, accent),
        },
        Mutation::SetStyle {
            id: TITLE_NODE,
            style: title_style(window.width, focused, accent),
        },
    ]
}

pub(super) fn window_size(viewport: Size) -> Size {
    let minimum_width = 240.min(viewport.width);
    let maximum_width = 760.min(viewport.width);
    let workspace_height = viewport.height.saturating_sub(TASKBAR_HEIGHT);
    let minimum_height = 180.min(workspace_height);
    let maximum_height = 520.min(workspace_height);
    Size {
        width: viewport
            .width
            .saturating_sub(80)
            .clamp(minimum_width, maximum_width),
        height: workspace_height
            .saturating_sub(80)
            .clamp(minimum_height, maximum_height),
    }
}

pub(super) fn next_accent(current: usize) -> usize {
    (current + 1) % ACCENTS.len()
}

fn shadow_style(window: Window) -> Style {
    flat_style(
        ui_rect(
            window.x + SHADOW,
            window.y + SHADOW,
            window.width,
            window.height,
        ),
        0x00404040,
    )
}

fn window_style(window: Window, focused: bool, accent: usize) -> Style {
    bordered_style(
        ui_rect(window.x, window.y, window.width, window.height),
        0x00c0c0c0,
        if focused { ACCENTS[accent] } else { 0x00808080 },
        2,
    )
}

fn title_style(width: usize, focused: bool, accent: usize) -> Style {
    flat_style(
        ui_rect(
            3,
            3,
            width.saturating_sub(6),
            TITLE_HEIGHT.saturating_sub(4),
        ),
        if focused { ACCENTS[accent] } else { 0x00808080 },
    )
}

fn flat_style(bounds: Rect, background: u32) -> Style {
    Style {
        bounds,
        background,
        ..Style::default()
    }
}

fn bordered_style(bounds: Rect, background: u32, border_color: u32, border_width: u8) -> Style {
    Style {
        bounds,
        background,
        border_color,
        border_width,
        ..Style::default()
    }
}

fn ui_rect(x: usize, y: usize, width: usize, height: usize) -> Rect {
    Rect::from_pixels(
        saturating_i32(x),
        saturating_i32(y),
        saturating_i32(width),
        saturating_i32(height),
    )
}

fn saturating_i32(value: usize) -> i32 {
    value.min(i32::MAX as usize) as i32
}
