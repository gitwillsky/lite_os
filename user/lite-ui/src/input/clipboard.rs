//! Native clipboard default actions for LiteUI text fields.

use std::error::Error;

use quickjs_runtime::Engine;
use serde_json::json;

use super::dispatch_listener;
use crate::{
    Interactions,
    display::Display,
    renderer::{Editable, Renderer},
};

#[derive(Clone, Copy)]
pub(crate) struct ClipboardPaste {
    node_id: u64,
    request_id: u64,
}

/// Resolves a native paste only when its original field still owns focus.
pub(super) fn apply_data(
    engine: &mut Engine,
    renderer: &Renderer,
    interactions: &mut Interactions,
    data: &display_proto::ClipboardData,
) -> Result<bool, Box<dyn Error>> {
    if !interactions
        .pending_clipboard_paste
        .is_some_and(|pending| pending.request_id == data.request_id)
    {
        return Ok(false);
    }
    let pending = interactions
        .pending_clipboard_paste
        .take()
        .expect("matched pending paste disappeared");
    if renderer.focused() == Some(pending.node_id)
        && let Some(editable) = interactions
            .hits
            .iter()
            .find(|hit| hit.node_id == pending.node_id)
            .and_then(|hit| hit.editable.as_ref())
        && let Some(on_input) = editable.on_input
    {
        let mut next = editable.value.clone();
        next.push_str(&data.text);
        dispatch_listener(engine, on_input, json!({ "type": "input", "value": next }))?;
    }
    Ok(true)
}

/// Applies Ctrl/Cmd+C/X/V for the complete append-only controlled value.
pub(super) fn apply_shortcut(
    engine: &mut Engine,
    interactions: &mut Interactions,
    display: &Display,
    node_id: u64,
    editable: &Editable,
    code: u32,
) -> Result<bool, Box<dyn Error>> {
    if !(interactions.modifiers.control || interactions.modifiers.super_key) {
        return Ok(false);
    }
    match code {
        46 => display.clipboard_write(editable.value.clone())?,
        45 => {
            display.clipboard_write(editable.value.clone())?;
            if let Some(on_input) = editable.on_input {
                dispatch_listener(engine, on_input, json!({ "type": "input", "value": "" }))?;
            }
        }
        47 => {
            let request_id = u64::MAX
                .checked_sub(interactions.native_clipboard_generation)
                .ok_or("native clipboard identity exhausted")?;
            interactions.native_clipboard_generation = interactions
                .native_clipboard_generation
                .checked_add(1)
                .ok_or("native clipboard identity exhausted")?;
            interactions.pending_clipboard_paste = Some(ClipboardPaste {
                node_id,
                request_id,
            });
            display.clipboard_read(request_id)?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}
