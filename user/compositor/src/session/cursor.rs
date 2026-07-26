use display_proto::SetCursorShape;

/// The cursor shape to apply when pointer focus moves from `prev` to `next`.
///
/// A real focus change returns the default arrow so a shape owned by the
/// surface being left never lingers on the next; an unchanged focus returns
/// `None` (leave the current shape alone).
pub(super) fn cursor_on_focus_change(prev: Option<u32>, next: Option<u32>) -> Option<u32> {
    (prev != next).then_some(display_proto::CURSOR_DEFAULT)
}

/// Resolves a `SetCursorShape` request into the shape to apply, if any.
///
/// - `Err(())`: the request names a different surface than the connection it
///   arrived on (a spoofed id) — the caller turns this into a protocol error.
/// - `Ok(Some(shape))`: `source_surface` holds pointer focus; apply its shape.
/// - `Ok(None)`: focus has since moved elsewhere; ignore the stale request.
pub(super) fn cursor_request(
    pointer_surface: Option<u32>,
    source_surface: u32,
    request: &SetCursorShape,
) -> Result<Option<u32>, ()> {
    if request.surface_id != source_surface {
        return Err(());
    }
    Ok((pointer_surface == Some(source_surface)).then_some(request.shape))
}

#[cfg(test)]
mod tests {
    use super::{cursor_on_focus_change, cursor_request};
    use display_proto::{CURSOR_DEFAULT, CURSOR_RESIZE_EW, SetCursorShape};

    fn request(surface_id: u32, shape: u32) -> SetCursorShape {
        SetCursorShape { surface_id, shape }
    }

    #[test]
    fn focus_change_resets_to_arrow_and_unchanged_focus_keeps_shape() {
        // Desktop resize edge (0) → app content (1): reset to arrow.
        assert_eq!(
            cursor_on_focus_change(Some(0), Some(1)),
            Some(CURSOR_DEFAULT)
        );
        // Off every surface: still resets.
        assert_eq!(cursor_on_focus_change(Some(0), None), Some(CURSOR_DEFAULT));
        // Same surface: leave the current shape untouched.
        assert_eq!(cursor_on_focus_change(Some(1), Some(1)), None);
    }

    #[test]
    fn cursor_request_applies_only_from_the_focused_surface() {
        // App 1 holds focus and requests EW resize for itself: applied.
        assert_eq!(
            cursor_request(Some(1), 1, &request(1, CURSOR_RESIZE_EW)),
            Ok(Some(CURSOR_RESIZE_EW))
        );
        // Desktop (0) request after focus already moved to app 1: ignored.
        assert_eq!(
            cursor_request(Some(1), 0, &request(0, CURSOR_RESIZE_EW)),
            Ok(None)
        );
    }

    #[test]
    fn cursor_request_rejects_a_spoofed_surface_id() {
        // App 1's connection claims to be surface 2: protocol error.
        assert_eq!(
            cursor_request(Some(1), 1, &request(2, CURSOR_DEFAULT)),
            Err(())
        );
    }
}
