//! Standard CSS cursor values lowered to compositor-owned fixed shapes.

pub(super) fn shape(value: Option<&str>) -> u32 {
    match value {
        Some("none") => display_proto::CURSOR_NONE,
        Some("pointer") => display_proto::CURSOR_POINTER,
        Some("n-resize" | "s-resize" | "ns-resize") => display_proto::CURSOR_RESIZE_NS,
        Some("e-resize" | "w-resize" | "ew-resize") => display_proto::CURSOR_RESIZE_EW,
        Some("ne-resize" | "sw-resize" | "nesw-resize") => display_proto::CURSOR_RESIZE_NESW,
        Some("nw-resize" | "se-resize" | "nwse-resize") => display_proto::CURSOR_RESIZE_NWSE,
        _ => display_proto::CURSOR_DEFAULT,
    }
}

#[cfg(test)]
mod tests {
    use super::shape;

    #[test]
    fn opposite_resize_edges_share_one_standard_shape() {
        for value in ["n-resize", "s-resize", "ns-resize"] {
            assert_eq!(shape(Some(value)), display_proto::CURSOR_RESIZE_NS);
        }
        for value in ["e-resize", "w-resize", "ew-resize"] {
            assert_eq!(shape(Some(value)), display_proto::CURSOR_RESIZE_EW);
        }
        for value in ["ne-resize", "sw-resize", "nesw-resize"] {
            assert_eq!(shape(Some(value)), display_proto::CURSOR_RESIZE_NESW);
        }
        for value in ["nw-resize", "se-resize", "nwse-resize"] {
            assert_eq!(shape(Some(value)), display_proto::CURSOR_RESIZE_NWSE);
        }
    }

    #[test]
    fn unsupported_cursor_falls_back_to_the_default_arrow() {
        assert_eq!(
            shape(Some("url(custom.cur)")),
            display_proto::CURSOR_DEFAULT
        );
    }

    #[test]
    fn none_maps_to_the_hidden_standard_shape() {
        assert_eq!(shape(Some("none")), display_proto::CURSOR_NONE);
    }
}
