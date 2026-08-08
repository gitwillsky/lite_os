//! Native selection state for standard single-line text controls.

use std::time::{Duration, Instant};

use super::{Editable, Renderer};
use crate::{font::CursorMove, keymap::TextEdit, style::Computed};

const CARET_BLINK_INTERVAL: Duration = Duration::from_millis(500);

/// Focus-scoped blink state for the single native text caret.
pub(super) struct CaretBlink {
    node_id: Option<u64>,
    visible: bool,
    deadline: Option<Instant>,
}

impl CaretBlink {
    pub(super) fn new() -> Self {
        Self {
            node_id: None,
            visible: false,
            deadline: None,
        }
    }

    pub(super) fn reconcile(&mut self, node_id: Option<u64>, now: Instant) -> bool {
        if self.node_id == node_id {
            return false;
        }
        let previous = self.visible_node();
        self.node_id = node_id;
        self.visible = node_id.is_some();
        self.deadline = node_id.map(|_| now + CARET_BLINK_INTERVAL);
        previous != self.visible_node()
    }

    pub(super) fn reset(&mut self, node_id: u64, now: Instant) -> bool {
        if self.node_id != Some(node_id) {
            return false;
        }
        let changed = !self.visible;
        self.visible = true;
        self.deadline = Some(now + CARET_BLINK_INTERVAL);
        changed
    }

    pub(super) fn advance(&mut self, now: Instant) -> bool {
        let Some(deadline) = self.deadline else {
            return false;
        };
        if deadline > now {
            return false;
        }
        self.visible = !self.visible;
        self.deadline = Some(now + CARET_BLINK_INTERVAL);
        true
    }

    pub(super) fn next_delay(&self, now: Instant) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    pub(super) fn visible_node(&self) -> Option<u64> {
        self.node_id.filter(|_| self.visible)
    }
}

/// One controlled input's browser-owned editing state.
#[derive(Clone, Copy, PartialEq)]
pub(super) struct State {
    anchor: usize,
    focus: usize,
    pub(super) scroll_x: f32,
}

impl State {
    fn collapsed(index: usize) -> Self {
        Self {
            anchor: index,
            focus: index,
            scroll_x: 0.0,
        }
    }

    fn ordered(self) -> (usize, usize) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }
}

impl Renderer {
    fn control_state(&mut self, node_id: u64, value: &str) -> &mut State {
        let state = self
            .text_controls
            .entry(node_id)
            .or_insert_with(|| State::collapsed(value.len()));
        state.anchor = valid_boundary(value, state.anchor);
        state.focus = valid_boundary(value, state.focus);
        state
    }

    /// Returns the current selection and keeps its caret visible in `width`.
    pub(super) fn control_geometry(
        &mut self,
        node_id: u64,
        value: &str,
        style: &Computed,
        width: usize,
    ) -> (usize, usize, f32) {
        let state = *self.control_state(node_id, value);
        let caret_x = self
            .font
            .control_selection_geometry(style, value, state.anchor, state.focus)
            .caret_x;
        let width = width as f32;
        let stored = self.control_state(node_id, value);
        if caret_x < stored.scroll_x {
            stored.scroll_x = caret_x;
        } else if caret_x > stored.scroll_x + width {
            stored.scroll_x = (caret_x - width).max(0.0);
        }
        (stored.anchor, stored.focus, stored.scroll_x)
    }

    /// Places or extends the shaped caret at a pointer coordinate.
    pub(crate) fn place_control_cursor(
        &mut self,
        node_id: u64,
        editable: &Editable,
        pointer_x: i32,
        extend: bool,
    ) -> bool {
        let local_x = (pointer_x - editable.text_origin_x).max(0) as f32;
        let next = self
            .font
            .control_cursor_from_point(&editable.style, &editable.value, local_x);
        let state = self.control_state(node_id, &editable.value);
        let old = (state.anchor, state.focus);
        if !extend {
            state.anchor = next;
        }
        state.focus = next;
        old != (state.anchor, state.focus)
    }

    /// Applies one visual cursor movement, optionally extending the selection.
    pub(crate) fn move_control_cursor(
        &mut self,
        node_id: u64,
        editable: &Editable,
        movement: CursorMove,
        extend: bool,
    ) -> bool {
        let state = *self.control_state(node_id, &editable.value);
        let next = if !extend && state.anchor != state.focus {
            let (start, end) = state.ordered();
            match movement {
                CursorMove::Previous | CursorMove::PreviousWord => start,
                CursorMove::Next | CursorMove::NextWord => end,
            }
        } else {
            self.font
                .move_control_cursor(&editable.style, &editable.value, state.focus, movement)
        };
        self.set_control_focus(node_id, &editable.value, next, extend)
    }

    /// Moves the caret to an exact byte boundary (Home/End/select-all).
    pub(crate) fn set_control_focus(
        &mut self,
        node_id: u64,
        value: &str,
        focus: usize,
        extend: bool,
    ) -> bool {
        let focus = valid_boundary(value, focus);
        let state = self.control_state(node_id, value);
        let old = (state.anchor, state.focus);
        if !extend {
            state.anchor = focus;
        }
        state.focus = focus;
        old != (state.anchor, state.focus)
    }

    /// Selects the entire controlled value.
    pub(crate) fn select_all_control(&mut self, node_id: u64, value: &str) -> bool {
        let state = self.control_state(node_id, value);
        let changed = (state.anchor, state.focus) != (0, value.len());
        state.anchor = 0;
        state.focus = value.len();
        changed
    }

    /// Returns selected text, or an empty string for a collapsed caret.
    pub(crate) fn selected_control_text(&mut self, node_id: u64, value: &str) -> String {
        let (start, end) = self.control_state(node_id, value).ordered();
        value[start..end].to_owned()
    }

    /// Replaces the selection or applies a Backspace at the shaped caret.
    pub(crate) fn edit_control(
        &mut self,
        node_id: u64,
        editable: &Editable,
        edit: TextEdit,
    ) -> String {
        let state = *self.control_state(node_id, &editable.value);
        let (mut start, end) = state.ordered();
        let insertion = match edit {
            TextEdit::Insert(character) => character.to_string(),
            TextEdit::Backspace => {
                if start == end {
                    start = self.font.move_control_cursor(
                        &editable.style,
                        &editable.value,
                        start,
                        CursorMove::Previous,
                    );
                }
                String::new()
            }
        };
        self.replace_control_range(node_id, &editable.value, start, end, &insertion)
    }

    /// Replaces the current selection with clipboard text.
    pub(crate) fn paste_control(&mut self, node_id: u64, value: &str, insertion: &str) -> String {
        let (start, end) = self.control_state(node_id, value).ordered();
        self.replace_control_range(node_id, value, start, end, insertion)
    }

    /// Deletes the current selection, used by Cut.
    pub(crate) fn delete_control_selection(&mut self, node_id: u64, value: &str) -> String {
        let (start, end) = self.control_state(node_id, value).ordered();
        self.replace_control_range(node_id, value, start, end, "")
    }

    fn replace_control_range(
        &mut self,
        node_id: u64,
        value: &str,
        start: usize,
        end: usize,
        insertion: &str,
    ) -> String {
        let mut next = String::with_capacity(value.len() - (end - start) + insertion.len());
        next.push_str(&value[..start]);
        next.push_str(insertion);
        next.push_str(&value[end..]);
        let caret = start + insertion.len();
        *self.control_state(node_id, &next) = State::collapsed(caret);
        next
    }
}

fn valid_boundary(value: &str, index: usize) -> usize {
    let mut index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{CARET_BLINK_INTERVAL, CaretBlink};

    #[test]
    fn caret_blink_activates_toggles_resets_and_stops_with_focus() {
        let start = Instant::now();
        let mut blink = CaretBlink::new();

        assert!(blink.reconcile(Some(7), start));
        assert_eq!(blink.visible_node(), Some(7));
        assert_eq!(blink.next_delay(start), Some(CARET_BLINK_INTERVAL));
        assert!(!blink.advance(start + CARET_BLINK_INTERVAL - Duration::from_millis(1)));
        assert!(blink.advance(start + CARET_BLINK_INTERVAL));
        assert_eq!(blink.visible_node(), None);

        let reset = start + CARET_BLINK_INTERVAL + Duration::from_millis(100);
        assert!(blink.reset(7, reset));
        assert_eq!(blink.visible_node(), Some(7));
        assert_eq!(blink.next_delay(reset), Some(CARET_BLINK_INTERVAL));

        assert!(blink.reconcile(None, reset));
        assert_eq!(blink.visible_node(), None);
        assert_eq!(blink.next_delay(reset), None);
    }
}
