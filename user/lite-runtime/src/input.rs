//! DOM-style input targeting over the latest rendered hit regions.

mod clipboard;
mod dispatch;

pub(super) use clipboard::ClipboardPaste;
#[cfg(test)]
pub(super) use dispatch::bubbling_listener_ids;
use dispatch::{
    dispatch_bubbling, dispatch_listener, dispatch_range_pointer, dispatch_range_value,
    dispatch_scroll,
};

use std::{
    error::Error,
    time::{Duration, Instant},
};

use quickjs_runtime::Engine;
use serde_json::json;

use crate::{
    Interactions, dispatch,
    display::{Display, Event},
    font::CursorMove,
    host::State,
    keymap,
    renderer::{self, Renderer},
};

/// Linux evdev `BTN_RIGHT`; the compositor forwards raw button codes and the
/// right button opens context menus rather than starting a drag or click.
const BTN_RIGHT: u32 = 273;

#[derive(Clone, Copy)]
pub(super) struct PointerCapture {
    /// Stable React host node that received pointer-down.
    ///
    /// Capturing callback ids instead would break after any React commit that
    /// replaces an inline handler: later motion would target a deleted id.
    pub(super) node_id: u64,
}

impl PointerCapture {
    fn hit(self, hits: &[renderer::HitRegion]) -> Option<&renderer::HitRegion> {
        hits.iter().find(|hit| hit.node_id == self.node_id)
    }

    #[cfg(test)]
    pub(super) fn move_listener(self, hits: &[renderer::HitRegion]) -> Option<u64> {
        self.hit(hits).and_then(|hit| hit.pointer_move)
    }

    #[cfg(test)]
    pub(super) fn up_listener(self, hits: &[renderer::HitRegion]) -> Option<u64> {
        self.hit(hits).and_then(|hit| hit.pointer_up)
    }
}

pub(super) fn apply_event(
    state: &State,
    engine: &mut Engine,
    renderer: &mut Renderer,
    interactions: &mut Interactions,
    display: &Display,
    event: Event,
) -> Result<(), Box<dyn Error>> {
    let (channel, payload) = match event {
        Event::AppOpened { surface_id, app_id } => {
            state.open_surface(surface_id, app_id.clone());
            (
                "desktop",
                json!({"type":"opened","surface":{"id":surface_id,"appId":app_id}}),
            )
        }
        Event::AppClosed { surface_id } => {
            state.close_surface(surface_id);
            ("desktop", json!({"type":"closed","surfaceId":surface_id}))
        }
        Event::SurfaceActivated { surface_id } => (
            "desktop",
            json!({"type":"activated","surfaceId":surface_id}),
        ),
        Event::MoveComplete { surface_id, x, y } => {
            // The compositor clamps the move destination to the authorized
            // limits. Clamp a stray negative from a race to the origin so the
            // native and React copies of the canonical bounds remain aligned.
            let x = x.max(0);
            let y = y.max(0);
            state.move_surface(surface_id, x as u32, y as u32)?;
            (
                "desktop",
                json!({"type":"moved","surfaceId":surface_id,"x":x,"y":y}),
            )
        }
        Event::ConfigureReady { .. } => {
            state.invalidate_composition();
            return Ok(());
        }
        Event::Configure(configure) => (
            "display",
            json!({"type":"configure","width":configure.width,"height":configure.height,"serial":configure.serial}),
        ),
        Event::OutputConfigure(_) => return Ok(()),
        Event::Pointer(pointer) => {
            dispatch_pointer(state, engine, renderer, interactions, display, pointer)?;
            return Ok(());
        }
        Event::Scroll(scroll) => {
            dispatch_scroll(state, engine, renderer, interactions, scroll)?;
            return Ok(());
        }
        Event::Key(key) => {
            if key.value != 0 {
                state.grant_media_playback();
            }
            dispatch_key(state, engine, renderer, interactions, display, key)?;
            return Ok(());
        }
        Event::ClipboardData(data) => {
            if clipboard::apply_data(engine, renderer, interactions, &data)? {
                return Ok(());
            }
            (
                "clipboard",
                json!({"requestId":data.request_id,"text":data.text}),
            )
        }
        Event::FrameDone | Event::Presented { .. } => return Ok(()),
        Event::Close => unreachable!("close exits before event dispatch"),
    };
    dispatch(engine, channel, payload)
}

/// Routes one key event. When an `<input>` is focused and still present in the
/// latest hits, `keydown` first bubbles from that target through its actual DOM
/// ancestors; the renderer then applies the button/range/text default action.
/// Otherwise the deepest global `onKeyDown` receives the transition.
fn dispatch_key(
    state: &State,
    engine: &mut Engine,
    renderer: &mut Renderer,
    interactions: &mut Interactions,
    display: &Display,
    key: display_proto::InputKey,
) -> Result<(), Box<dyn Error>> {
    // The focused input must still exist in the current scene; a React commit
    // may have removed it, in which case focus falls away and the key routes
    // globally (never to a stale node).
    let focused = renderer.focused().and_then(|node_id| {
        interactions
            .hits
            .iter()
            .find(|hit| hit.node_id == node_id)
            .map(|hit| {
                (
                    node_id,
                    hit.editable.clone(),
                    hit.range,
                    hit.button,
                    hit.click,
                )
            })
    });
    if let Some((node_id, editable, range, button, click)) = focused {
        // Fold modifiers first; a modifier key produces no text itself.
        if interactions.modifiers.apply(key.code, key.value) {
            return Ok(());
        }
        if key.value != 0 {
            dispatch_bubbling(
                engine,
                &interactions.hits,
                Some(node_id),
                |hit| hit.key_down,
                json!({
                    "type":"key",
                    "code":key.code,
                    "value":key.value,
                    "modifiers":key.modifiers
                }),
            )?;
        }
        if button {
            let activation_key = matches!(key.code, 28 | 57); // KEY_ENTER / KEY_SPACE
            if activation_key {
                let pressed = key.value != 0;
                if renderer.set_active_target(pressed.then_some(node_id)) {
                    state.invalidate_scene();
                }
                let invokes =
                    (key.code == 28 && key.value == 1) || (key.code == 57 && key.value == 0);
                if invokes && let Some(click) = click {
                    dispatch_listener(
                        engine,
                        click,
                        json!({"type":"click","detail":0,"keyboard":true}),
                    )?;
                }
            }
            return Ok(());
        }
        if let Some(range) = range {
            if key.value != 0 {
                let direction = match key.code {
                    103 | 106 => Some(1),  // KEY_UP / KEY_RIGHT
                    105 | 108 => Some(-1), // KEY_LEFT / KEY_DOWN
                    _ => None,
                };
                if let Some(direction) = direction {
                    dispatch_range_value(engine, range, range.stepped(direction))?;
                }
            }
            return Ok(());
        }
        let editable = editable.expect("focused non-range input is editable");
        if key.value != 0
            && clipboard::apply_shortcut(
                engine,
                renderer,
                interactions,
                display,
                node_id,
                &editable,
                key.code,
            )?
        {
            state.invalidate_scene();
            return Ok(());
        }
        if key.value != 0 {
            let movement = match key.code {
                105 if interactions.modifiers.control => Some(CursorMove::PreviousWord),
                106 if interactions.modifiers.control => Some(CursorMove::NextWord),
                105 => Some(CursorMove::Previous),
                106 => Some(CursorMove::Next),
                _ => None,
            };
            if let Some(movement) = movement {
                if renderer.move_control_cursor(
                    node_id,
                    &editable,
                    movement,
                    interactions.modifiers.shift,
                ) {
                    state.invalidate_scene();
                }
                return Ok(());
            }
            let edge = match key.code {
                102 => Some(0),                    // KEY_HOME
                107 => Some(editable.value.len()), // KEY_END
                _ => None,
            };
            if let Some(edge) = edge {
                if renderer.set_control_focus(
                    node_id,
                    &editable.value,
                    edge,
                    interactions.modifiers.shift,
                ) {
                    state.invalidate_scene();
                }
                return Ok(());
            }
        }
        if let Some(edit) = keymap::text_edit(key.code, key.value, interactions.modifiers) {
            if let Some(on_input) = editable.on_input {
                let next = renderer.edit_control(node_id, &editable, edit);
                dispatch_listener(engine, on_input, json!({ "type": "input", "value": next }))?;
            }
            return Ok(());
        }
        return Ok(());
    }
    if let Some(listener) = interactions.key_listener {
        dispatch_listener(
            engine,
            listener,
            json!({"type":"key","code":key.code,"value":key.value,"modifiers":key.modifiers}),
        )?;
    }
    Ok(())
}

fn dispatch_pointer(
    state: &State,
    engine: &mut Engine,
    renderer: &mut Renderer,
    interactions: &mut Interactions,
    display: &Display,
    pointer: display_proto::InputPointer,
) -> Result<(), Box<dyn Error>> {
    interactions.pointer_position = Some((pointer.x, pointer.y));
    if pointer.phase == display_proto::PointerPhase::Down {
        state.grant_media_playback();
    }
    match pointer.phase {
        display_proto::PointerPhase::Down if renderer.scrollbar_at(pointer.x, pointer.y) => {
            let pseudo_changed = renderer.set_hover_target(None) | renderer.set_active_target(None);
            let changed = if pointer.button == BTN_RIGHT {
                false
            } else {
                renderer.scrollbar_pointer_down(pointer.x, pointer.y).1
            };
            interactions.native_scroll_pointer = true;
            if changed || pseudo_changed {
                state.invalidate_scene();
            }
            return Ok(());
        }
        display_proto::PointerPhase::Motion if interactions.native_scroll_pointer => {
            let scroll_changed = renderer.scrollbar_pointer_move(pointer.x, pointer.y);
            let pseudo_changed = renderer.set_hover_target(None);
            if scroll_changed || pseudo_changed {
                state.invalidate_scene();
            }
            return Ok(());
        }
        display_proto::PointerPhase::Up if interactions.native_scroll_pointer => {
            interactions.native_scroll_pointer = false;
            renderer.scrollbar_pointer_up();
            if renderer.set_active_target(None) {
                state.invalidate_scene();
            }
            return Ok(());
        }
        _ => {}
    }
    let inside = |hit: &renderer::HitRegion| {
        pointer.x as f32 >= hit.x
            && pointer.y as f32 >= hit.y
            && (pointer.x as f32) < hit.x + hit.width
            && (pointer.y as f32) < hit.y + hit.height
    };
    let payload = json!({
        "type":"pointer",
        "phase": match pointer.phase {
            display_proto::PointerPhase::Motion => "motion",
            display_proto::PointerPhase::Down => "down",
            display_proto::PointerPhase::Up => "up",
        },
        "x":pointer.x,
        "y":pointer.y,
        "button":pointer.button,
        "buttons":pointer.buttons,
        "serial":pointer.serial
    });
    let css_target = interactions
        .hits
        .iter()
        .rev()
        .find(|hit| inside(hit))
        .map(|hit| hit.node_id);
    match pointer.phase {
        display_proto::PointerPhase::Motion => {
            if renderer.set_hover_target(css_target) {
                state.invalidate_scene();
            }
        }
        display_proto::PointerPhase::Down if pointer.button != BTN_RIGHT => {
            if renderer.set_active_target(css_target) {
                state.invalidate_scene();
            }
        }
        display_proto::PointerPhase::Up if pointer.button != BTN_RIGHT => {
            if renderer.set_active_target(None) {
                state.invalidate_scene();
            }
        }
        _ => {}
    }
    if renderer.scrollbar_at(pointer.x, pointer.y) {
        if pointer.phase == display_proto::PointerPhase::Motion {
            if let Some(old) = interactions.hovered
                && let Some(leave) = interactions
                    .hits
                    .iter()
                    .find(|hit| hit.node_id == old)
                    .and_then(|hit| hit.pointer_leave)
            {
                dispatch_listener(engine, leave, payload)?;
            }
            interactions.hovered = None;
            if renderer.set_hover_target(None) {
                state.invalidate_scene();
            }
            if interactions.cursor_shape != 0 {
                display.set_cursor_shape(0)?;
                interactions.cursor_shape = 0;
            }
        }
        return Ok(());
    }
    match pointer.phase {
        display_proto::PointerPhase::Down => {
            if pointer.button == BTN_RIGHT {
                dispatch_bubbling(
                    engine,
                    &interactions.hits,
                    css_target,
                    |hit| hit.context_menu,
                    payload.clone(),
                )?;
            } else {
                // 焦点跟随左键按下（标准 DOM 语义）：文本与 range `<input>` 都可聚焦；
                // disabled range 不可聚焦。焦点变化需重绘光标/滑块焦点框。
                let focus_target = interactions
                    .hits
                    .iter()
                    .rev()
                    .filter(|hit| inside(hit))
                    .find(|hit| {
                        hit.editable.is_some()
                            || hit.range.is_some_and(|range| !range.disabled())
                            || hit.button
                    })
                    .map(|hit| hit.node_id);
                if renderer.set_focus(focus_target) {
                    state.invalidate_scene();
                }
                let text_target = interactions
                    .hits
                    .iter()
                    .rev()
                    .filter(|hit| inside(hit))
                    .find(|hit| hit.editable.is_some())
                    .cloned();
                if let Some(hit) = text_target {
                    if renderer.place_control_cursor(
                        hit.node_id,
                        hit.editable.as_ref().expect("text input hit"),
                        pointer.x,
                        interactions.modifiers.shift,
                    ) {
                        state.invalidate_scene();
                    }
                    interactions.pointer_capture = Some(PointerCapture {
                        node_id: hit.node_id,
                    });
                }
                let range_target = interactions
                    .hits
                    .iter()
                    .rev()
                    .filter(|hit| inside(hit))
                    .find(|hit| hit.range.is_some_and(|range| !range.disabled()))
                    .cloned();
                let range_captured = range_target.is_some();
                if let Some(hit) = range_target {
                    dispatch_range_pointer(engine, &hit, pointer.x)?;
                    interactions.pointer_capture = Some(PointerCapture {
                        node_id: hit.node_id,
                    });
                }
                if dispatch_bubbling(
                    engine,
                    &interactions.hits,
                    css_target,
                    |hit| hit.pointer_down,
                    payload.clone(),
                )? && !range_captured
                {
                    interactions.pointer_capture = Some(PointerCapture {
                        node_id: css_target.expect("bubbling route has a target"),
                    });
                }
            }
        }
        display_proto::PointerPhase::Up => {
            if let Some(capture) = interactions.pointer_capture.take() {
                if let Some(hit) = capture.hit(&interactions.hits) {
                    if let Some(editable) = &hit.editable
                        && renderer.place_control_cursor(hit.node_id, editable, pointer.x, true)
                    {
                        state.invalidate_scene();
                    }
                    if hit.range.is_some_and(|range| !range.disabled()) {
                        dispatch_range_pointer(engine, hit, pointer.x)?;
                    }
                    dispatch_bubbling(
                        engine,
                        &interactions.hits,
                        Some(hit.node_id),
                        |candidate| candidate.pointer_up,
                        payload.clone(),
                    )?;
                }
            }
            if pointer.button != BTN_RIGHT {
                let click_payload = json!({
                    "type":"click",
                    "detail":1,
                    "x":pointer.x,
                    "y":pointer.y,
                    "button":pointer.button,
                    "buttons":pointer.buttons,
                    "serial":pointer.serial
                });
                dispatch_bubbling(
                    engine,
                    &interactions.hits,
                    css_target,
                    |hit| hit.click,
                    click_payload.clone(),
                )?;
                let now = Instant::now();
                let double = interactions.last_click.is_some_and(|(at, x, y)| {
                    now.duration_since(at) <= Duration::from_millis(500)
                        && (x - pointer.x).abs() <= 4
                        && (y - pointer.y).abs() <= 4
                });
                if double {
                    dispatch_bubbling(
                        engine,
                        &interactions.hits,
                        css_target,
                        |hit| hit.double_click,
                        json!({
                            "type":"dblclick",
                            "detail":2,
                            "x":pointer.x,
                            "y":pointer.y,
                            "button":pointer.button,
                            "buttons":pointer.buttons,
                            "serial":pointer.serial
                        }),
                    )?;
                    interactions.last_click = None;
                } else {
                    interactions.last_click = Some((now, pointer.x, pointer.y));
                }
            }
        }
        display_proto::PointerPhase::Motion => {
            if let Some(capture) = interactions.pointer_capture {
                if let Some(hit) = capture.hit(&interactions.hits) {
                    if let Some(editable) = &hit.editable
                        && renderer.place_control_cursor(hit.node_id, editable, pointer.x, true)
                    {
                        state.invalidate_scene();
                    }
                    if hit.range.is_some_and(|range| !range.disabled()) {
                        dispatch_range_pointer(engine, hit, pointer.x)?;
                    }
                    dispatch_bubbling(
                        engine,
                        &interactions.hits,
                        Some(hit.node_id),
                        |candidate| candidate.pointer_move,
                        payload,
                    )?;
                }
            } else {
                let next = interactions
                    .hits
                    .iter()
                    .rev()
                    .find(|hit| {
                        inside(hit)
                            && (hit.pointer_enter.is_some()
                                || hit.pointer_leave.is_some()
                                || hit.pointer_move.is_some())
                    })
                    .map(|hit| hit.node_id);
                if next != interactions.hovered {
                    if let Some(old) = interactions.hovered
                        && let Some(leave) = interactions
                            .hits
                            .iter()
                            .find(|hit| hit.node_id == old)
                            .and_then(|hit| hit.pointer_leave)
                    {
                        dispatch_listener(engine, leave, payload.clone())?;
                    }
                    if let Some(new) = next
                        && let Some(enter) = interactions
                            .hits
                            .iter()
                            .find(|hit| hit.node_id == new)
                            .and_then(|hit| hit.pointer_enter)
                    {
                        dispatch_listener(engine, enter, payload.clone())?;
                    }
                    interactions.hovered = next;
                }
                dispatch_bubbling(
                    engine,
                    &interactions.hits,
                    css_target,
                    |hit| hit.pointer_move,
                    payload,
                )?;
                reconcile_cursor(interactions, display)?;
            }
        }
    }
    Ok(())
}

/// Re-evaluates the standard CSS cursor at the latest routed pointer position.
///
/// This is called both after pointer motion and after a rendered DOM/style
/// change, matching the Web behavior where cursor changes do not require the
/// user to jiggle the pointing device.
pub(crate) fn reconcile_cursor(
    interactions: &mut Interactions,
    display: &Display,
) -> Result<(), Box<dyn Error>> {
    let Some((x, y)) = interactions.pointer_position else {
        return Ok(());
    };
    let shape = interactions
        .hits
        .iter()
        .rev()
        .find(|hit| {
            x as f32 >= hit.x
                && y as f32 >= hit.y
                && (x as f32) < hit.x + hit.width
                && (y as f32) < hit.y + hit.height
        })
        .map(|hit| hit.cursor)
        .unwrap_or(display_proto::CURSOR_DEFAULT);
    if shape != interactions.cursor_shape {
        display.set_cursor_shape(shape)?;
        interactions.cursor_shape = shape;
    }
    Ok(())
}
