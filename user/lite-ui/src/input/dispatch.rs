//! DOM event route dispatch and form-control default actions.

use std::error::Error;

use quickjs_runtime::Engine;
use serde_json::json;

use crate::{
    Interactions,
    host::State,
    renderer::{self, Renderer},
};

pub(super) fn dispatch_range_pointer(
    engine: &mut Engine,
    hit: &renderer::HitRegion,
    pointer_x: i32,
) -> Result<(), Box<dyn Error>> {
    let range = hit.range.expect("range hit");
    let value = range.value_at(pointer_x as f32, hit.x, hit.width);
    dispatch_range_value(engine, range, value)
}

pub(super) fn dispatch_range_value(
    engine: &mut Engine,
    range: renderer::RangeInput,
    value: f64,
) -> Result<(), Box<dyn Error>> {
    if value == range.value() {
        return Ok(());
    }
    if let Some(on_input) = range.on_input() {
        dispatch_listener(
            engine,
            on_input,
            json!({
                "type": "input",
                "value": renderer::RangeInput::string_value(value)
            }),
        )?;
    }
    Ok(())
}

pub(super) fn dispatch_scroll(
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
    let target = interactions
        .hits
        .iter()
        .rev()
        .find(|hit| inside(hit))
        .map(|hit| hit.node_id);
    dispatch_bubbling(
        engine,
        &interactions.hits,
        target,
        |hit| hit.wheel,
        json!({
            "type":"wheel",
            "x":scroll.x,
            "y":scroll.y,
            "deltaX":scroll.delta_x,
            "deltaY":scroll.delta_y,
            "deltaMode":0
        }),
    )?;
    if renderer.scroll_wheel(scroll.x, scroll.y, scroll.delta_x, scroll.delta_y) {
        state.invalidate_scene();
    }
    Ok(())
}

pub(super) fn dispatch_listener(
    engine: &mut Engine,
    listener: u64,
    payload: serde_json::Value,
) -> Result<(), Box<dyn Error>> {
    let payload = serde_json::to_string(&payload)?;
    let script = format!("globalThis.__liteDispatch({listener},{payload});");
    engine.evaluate("lite-ui-listener.js", script.as_bytes())?;
    Ok(())
}

/// Dispatches an event from its deepest painted target through actual DOM
/// ancestors. The complete route enters JavaScript once so `stopPropagation()`
/// can halt later ancestors and React commits once after the discrete event.
pub(super) fn dispatch_bubbling(
    engine: &mut Engine,
    hits: &[renderer::HitRegion],
    target: Option<u64>,
    listener: fn(&renderer::HitRegion) -> Option<u64>,
    payload: serde_json::Value,
) -> Result<bool, Box<dyn Error>> {
    let route = bubbling_listener_ids(hits, target, listener);
    if route.is_empty() {
        return Ok(false);
    }
    if let [listener] = route.as_slice() {
        dispatch_listener(engine, *listener, payload)?;
        return Ok(true);
    }
    let route = serde_json::to_string(&route)?;
    let payload = serde_json::to_string(&payload)?;
    let script = format!("globalThis.__liteDispatch({route},{payload});");
    engine.evaluate("lite-ui-listener.js", script.as_bytes())?;
    Ok(true)
}

pub(crate) fn bubbling_listener_ids(
    hits: &[renderer::HitRegion],
    target: Option<u64>,
    listener: fn(&renderer::HitRegion) -> Option<u64>,
) -> Vec<u64> {
    let mut current = target;
    let mut route = Vec::new();
    while let Some(node_id) = current {
        let Some(hit) = hits.iter().find(|hit| hit.node_id == node_id) else {
            break;
        };
        if let Some(listener) = listener(hit) {
            route.push(listener);
        }
        current = hit.parent_node_id;
    }
    route
}
