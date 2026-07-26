//! DOM-style input targeting over the latest rendered hit regions.

use std::{
    error::Error,
    time::{Duration, Instant},
};

use quickjs_runtime::Engine;
use serde_json::json;

use crate::{
    Interactions, dispatch,
    display::{Display, Event},
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

    pub(super) fn move_listener(self, hits: &[renderer::HitRegion]) -> Option<u64> {
        self.hit(hits).and_then(|hit| hit.pointer_move)
    }

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
        Event::Pointer(pointer) => {
            dispatch_pointer(state, engine, renderer, interactions, display, pointer)?;
            return Ok(());
        }
        Event::Scroll(scroll) => {
            dispatch_scroll(state, engine, renderer, interactions, scroll)?;
            return Ok(());
        }
        Event::Key(key) => {
            dispatch_key(engine, renderer, interactions, key)?;
            return Ok(());
        }
        Event::FrameDone => return Ok(()),
        Event::Close => unreachable!("close exits before event dispatch"),
    };
    dispatch(engine, channel, payload)
}

/// Routes one key event. When an `<input>` is focused and still present in the
/// latest hits, the renderer's keymap turns the key into a text edit (dispatched
/// to `onInput` with the new controlled value) or an `onKeyDown` (Enter/Esc/
/// arrows). Otherwise the deepest global `onKeyDown` receives it — preserving
/// the terminal and desktop-Escape behavior when nothing is focused.
fn dispatch_key(
    engine: &mut Engine,
    renderer: &mut Renderer,
    interactions: &mut Interactions,
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
            .and_then(|hit| hit.editable.clone().map(|editable| (node_id, editable)))
    });
    if let Some((node_id, editable)) = focused {
        // Fold modifiers first; a modifier key produces no text itself.
        if interactions.modifiers.apply(key.code, key.value) {
            return Ok(());
        }
        if let Some(edit) = keymap::text_edit(key.code, key.value, interactions.modifiers) {
            if let Some(on_input) = editable.on_input {
                let next = apply_text_edit(&editable.value, edit);
                dispatch_listener(engine, on_input, json!({ "type": "input", "value": next }))?;
            }
            return Ok(());
        }
        // Non-text keys (Enter/Esc/Tab/arrows) go to the input's own onKeyDown
        // so the field can commit or cancel; only on a press edge.
        if key.value != 0
            && let Some(on_key) = interactions
                .hits
                .iter()
                .find(|hit| hit.node_id == node_id)
                .and_then(|hit| hit.key_down)
        {
            dispatch_listener(
                engine,
                on_key,
                json!({"type":"key","code":key.code,"value":key.value,"modifiers":key.modifiers}),
            )?;
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

/// Applies one `TextEdit` to a controlled value, returning the new string that
/// the input's `onInput` handler will store (append a char / delete the last).
fn apply_text_edit(value: &str, edit: keymap::TextEdit) -> String {
    match edit {
        keymap::TextEdit::Insert(character) => {
            let mut next = value.to_owned();
            next.push(character);
            next
        }
        keymap::TextEdit::Backspace => {
            let mut next = value.to_owned();
            next.pop();
            next
        }
    }
}

fn dispatch_pointer(
    state: &State,
    engine: &mut Engine,
    renderer: &mut Renderer,
    interactions: &mut Interactions,
    display: &Display,
    pointer: display_proto::InputPointer,
) -> Result<(), Box<dyn Error>> {
    match pointer.phase {
        display_proto::PointerPhase::Down if renderer.scrollbar_at(pointer.x, pointer.y) => {
            let changed = if pointer.button == BTN_RIGHT {
                false
            } else {
                renderer.scrollbar_pointer_down(pointer.x, pointer.y).1
            };
            interactions.native_scroll_pointer = true;
            if changed {
                state.invalidate_scene();
            }
            return Ok(());
        }
        display_proto::PointerPhase::Motion if interactions.native_scroll_pointer => {
            if renderer.scrollbar_pointer_move(pointer.x, pointer.y) {
                state.invalidate_scene();
            }
            return Ok(());
        }
        display_proto::PointerPhase::Up if interactions.native_scroll_pointer => {
            interactions.native_scroll_pointer = false;
            renderer.scrollbar_pointer_up();
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
                if let Some(listener) = interactions
                    .hits
                    .iter()
                    .rev()
                    .filter(|hit| inside(hit))
                    .filter_map(|hit| hit.context_menu)
                    .next()
                {
                    dispatch_listener(engine, listener, payload.clone())?;
                }
            } else {
                // 焦点跟随左键按下（标准 DOM 语义）：命中最顶层可编辑 `<input>` 则聚焦它，
                // 否则清焦点。焦点变化需重绘以移动/隐藏文本光标。渲染器是焦点单一 owner。
                let focus_target = interactions
                    .hits
                    .iter()
                    .rev()
                    .filter(|hit| inside(hit))
                    .find(|hit| hit.editable.is_some())
                    .map(|hit| hit.node_id);
                if renderer.set_focus(focus_target) {
                    state.invalidate_scene();
                }
                if let Some(hit) = interactions
                    .hits
                    .iter()
                    .rev()
                    .filter(|hit| inside(hit))
                    .find(|hit| hit.pointer_down.is_some())
                {
                    dispatch_listener(
                        engine,
                        hit.pointer_down.expect("filtered pointer listener"),
                        payload.clone(),
                    )?;
                    interactions.pointer_capture = Some(PointerCapture {
                        node_id: hit.node_id,
                    });
                }
            }
        }
        display_proto::PointerPhase::Up => {
            if let Some(capture) = interactions.pointer_capture.take()
                && let Some(listener) = capture.up_listener(&interactions.hits)
            {
                dispatch_listener(engine, listener, payload.clone())?;
            }
            if pointer.button != BTN_RIGHT {
                if let Some(listener) = interactions
                    .hits
                    .iter()
                    .rev()
                    .filter(|hit| inside(hit))
                    .filter_map(|hit| hit.click)
                    .next()
                {
                    dispatch_listener(engine, listener, payload.clone())?;
                }
                let now = Instant::now();
                let double = interactions.last_click.is_some_and(|(at, x, y)| {
                    now.duration_since(at) <= Duration::from_millis(500)
                        && (x - pointer.x).abs() <= 4
                        && (y - pointer.y).abs() <= 4
                });
                if double {
                    if let Some(listener) = interactions
                        .hits
                        .iter()
                        .rev()
                        .filter(|hit| inside(hit))
                        .filter_map(|hit| hit.double_click)
                        .next()
                    {
                        dispatch_listener(engine, listener, payload.clone())?;
                    }
                    interactions.last_click = None;
                } else {
                    interactions.last_click = Some((now, pointer.x, pointer.y));
                }
            }
        }
        display_proto::PointerPhase::Motion => {
            if let Some(listener) = interactions
                .pointer_capture
                .and_then(|capture| capture.move_listener(&interactions.hits))
            {
                dispatch_listener(engine, listener, payload)?;
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
                if let Some(mv) = next.and_then(|node_id| {
                    interactions
                        .hits
                        .iter()
                        .find(|hit| hit.node_id == node_id)
                        .and_then(|hit| hit.pointer_move)
                }) {
                    dispatch_listener(engine, mv, payload)?;
                }
                let shape = interactions
                    .hits
                    .iter()
                    .rev()
                    .find(|hit| inside(hit))
                    .map(|hit| hit.cursor)
                    .unwrap_or(0);
                if shape != interactions.cursor_shape {
                    display.set_cursor_shape(shape)?;
                    interactions.cursor_shape = shape;
                }
            }
        }
    }
    Ok(())
}

fn dispatch_scroll(
    state: &State,
    engine: &mut Engine,
    renderer: &mut Renderer,
    interactions: &mut Interactions,
    scroll: display_proto::InputScroll,
) -> Result<(), Box<dyn Error>> {
    let inside = |hit: &renderer::HitRegion| {
        scroll.x as f32 >= hit.x
            && scroll.y as f32 >= hit.y
            && (scroll.x as f32) < hit.x + hit.width
            && (scroll.y as f32) < hit.y + hit.height
    };
    if let Some(listener) = interactions
        .hits
        .iter()
        .rev()
        .filter(|hit| inside(hit))
        .filter_map(|hit| hit.wheel)
        .next()
    {
        dispatch_listener(
            engine,
            listener,
            json!({
                "type":"wheel",
                "x":scroll.x,
                "y":scroll.y,
                "deltaX":scroll.delta_x,
                "deltaY":scroll.delta_y,
                "deltaMode":0
            }),
        )?;
    }
    if renderer.scroll_wheel(scroll.x, scroll.y, scroll.delta_x, scroll.delta_y) {
        state.invalidate_scene();
    }
    Ok(())
}

fn dispatch_listener(
    engine: &mut Engine,
    listener: u64,
    payload: serde_json::Value,
) -> Result<(), Box<dyn Error>> {
    let payload = serde_json::to_string(&payload)?;
    let script = format!("globalThis.__liteDispatch({listener},{payload});");
    engine.evaluate("lite-ui-listener.js", script.as_bytes())?;
    Ok(())
}
