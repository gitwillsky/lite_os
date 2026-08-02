//! Input routing against the last page-flip-complete scene.

use std::{io, os::unix::net::UnixStream};

use display_proto::{
    InputKey, InputPointer, InputScroll, MoveBegin, MoveComplete, PointerPhase, Rect,
    SurfaceActivated, send_message,
};

use super::accelerator;
use super::buffers::Owner;
use super::{MoveGrab, PointerCapture, Session, invalid};

impl Session {
    pub(super) fn clear_pointer_capture(&mut self, surface_id: Option<u32>) {
        if self
            .pointer_capture
            .is_some_and(|capture| surface_id.is_none_or(|id| capture.surface_id == id))
        {
            self.pointer_capture = None;
        }
        if self
            .move_grab
            .is_some_and(|grab| surface_id.is_none_or(|id| grab.surface_id == id))
        {
            let grab = self.move_grab.take().expect("matching move grab");
            self.move_damage = None;
            if surface_id.is_some() {
                self.move_underlays.remove(&grab.surface_id);
                self.buffers.values.remove(&grab.underlay_buffer_id);
            }
        }
    }

    /// Tells the desktop that a foreign surface was pressed so it can restack.
    fn notify_surface_activated(&self, surface_id: u32) -> io::Result<()> {
        let mut bytes = [0u8; 24];
        let message = SurfaceActivated { surface_id }
            .encode(&mut bytes)
            .ok_or_else(|| io::Error::other("surface-activated encoding failed"))?;
        send_message(self.desktop_stream()?, message)
    }

    /// Routes one pointer transition against the last presented scene.
    pub fn route_pointer(
        &mut self,
        x: i32,
        y: i32,
        phase: PointerPhase,
        button: u32,
        buttons: u32,
        serial: u64,
    ) -> io::Result<()> {
        let hit = self.routing.iter().rev().find(|node| {
            node.input
                .iter()
                .any(|rectangle| contains(*rectangle, x, y))
        });
        // The surface actually under the pointer, regardless of any capture. It
        // drives pointer-focus (and thus which surface owns the cursor shape),
        // and is where focus returns once a capture releases.
        let hover_surface = hit.map(|node| node.surface_id);
        let target = self
            .pointer_capture
            .map(|capture| (capture.surface_id, capture.window_group, capture.bounds))
            .or_else(|| hit.map(|target| (target.surface_id, target.window_group, target.bounds)));
        let Some((surface_id, window_group, bounds)) = target else {
            return Ok(());
        };
        if phase == PointerPhase::Down {
            self.pointer_capture = Some(PointerCapture {
                surface_id,
                window_group,
                bounds,
                serial,
                down: (x, y),
            });
            // A press on a foreign app surface routes only to that app, so the
            // desktop never sees it and cannot restack. Tell the desktop which
            // surface was pressed so it can raise and focus that window; the
            // desktop owns z-order (surface_id 0 is the desktop itself).
            //
            // Skip the notify when this surface already holds focus: repeated
            // presses (double-clicks, drag-select) on the active window would
            // otherwise flood the desktop with restack messages it must ignore.
            //
            // A failed notify must NOT abort pointer routing. This message goes
            // to the desktop stream, but the pointer event itself goes to the
            // app; if the desktop socket is momentarily blocked, dropping one
            // restack hint is recoverable, whereas propagating the error tears
            // down the whole compositor (route_pointer is `?`-chained to run()).
            if surface_id != 0
                && surface_id != self.focused_surface
                && let Err(error) = self.notify_surface_activated(surface_id)
            {
                eprintln!("compositor: surface-activated notify dropped: {error}");
            }
        }
        // Reconcile against the physical button state before touching the
        // grab. A pointer-Up can be lost — released off every hit region, or
        // dropped when the evdev queue coalesces — leaving a move grab that
        // tracks the pointer forever with no release in sight. Any Motion whose
        // button mask shows all buttons already up is proof the press ended
        // without an Up we routed, so retire the grab exactly as a real Up
        // would: send MoveComplete (idempotent once `ending`) and drop capture.
        // Done before `update_move` so the freeze that `ending` imposes on the
        // offset (see `next_move_offset`) takes effect on this same event.
        if phase == PointerPhase::Motion
            && buttons == 0
            && self.move_grab.is_some_and(|grab| !grab.ending)
        {
            self.finish_move()?;
            self.pointer_capture = None;
            self.set_pointer_surface(hover_surface);
            return Ok(());
        }
        if matches!(phase, PointerPhase::Motion | PointerPhase::Up) {
            self.update_move(x, y);
        }
        if phase == PointerPhase::Motion && self.move_grab.is_some() {
            // The compositor now owns this continuous transition. Forwarding
            // redundant motion would wake QuickJS and run the titlebar listener
            // even though React intentionally performs no canonical update.
            return Ok(());
        }
        // A free (uncaptured) motion establishes pointer focus on the hovered
        // surface, resetting the cursor to arrow on any surface change so a
        // shape set over one surface never lingers on the next.
        if phase == PointerPhase::Motion && self.pointer_capture.is_none() {
            self.set_pointer_surface(hover_surface);
        }
        let scale = display_proto::DEVICE_SCALE_FACTOR as i32;
        let event = InputPointer {
            surface_id,
            serial,
            phase,
            button,
            buttons,
            x: (x - bounds.x) / scale,
            y: (y - bounds.y) / scale,
        };
        let mut bytes = [0u8; 64];
        let message = event
            .encode(&mut bytes)
            .ok_or_else(|| io::Error::other("pointer encoding failed"))?;
        let result = send_message(self.target_stream(surface_id)?, message);
        if phase == PointerPhase::Up {
            self.finish_move()?;
            self.pointer_capture = None;
            // Capture ended: pointer focus (and cursor ownership) returns to the
            // surface actually under the pointer, resetting the cursor if it
            // differs from the captured surface.
            self.set_pointer_surface(hover_surface);
        }
        result
    }

    /// Routes one mouse-wheel scroll against the last presented scene.
    ///
    /// Mirrors `route_pointer`'s hit-test and surface-local translation, but a
    /// scroll never captures, restacks, or drives a move grab: it is delivered
    /// to the surface directly under the current pointer position, or to the
    /// desktop (surface zero) when no app surface is hit.
    pub fn route_scroll(
        &mut self,
        x: i32,
        y: i32,
        delta_x: i32,
        delta_y: i32,
        serial: u64,
    ) -> io::Result<()> {
        let hit = self.routing.iter().rev().find(|node| {
            node.input
                .iter()
                .any(|rectangle| contains(*rectangle, x, y))
        });
        let (surface_id, bounds) = match hit {
            Some(target) => (target.surface_id, target.bounds),
            None => (
                0,
                Rect {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                },
            ),
        };
        let scale = display_proto::DEVICE_SCALE_FACTOR as i32;
        let event = InputScroll {
            surface_id,
            serial,
            x: (x - bounds.x) / scale,
            y: (y - bounds.y) / scale,
            delta_x,
            delta_y,
        };
        let mut bytes = [0u8; 64];
        let message = event
            .encode(&mut bytes)
            .ok_or_else(|| io::Error::other("scroll encoding failed"))?;
        send_message(self.target_stream(surface_id)?, message)
    }

    /// Starts the one compositor-side move authorized by the matching pointer-down.
    ///
    /// `Ok(Some(error))` is a recoverable authorization race. Receipt of a valid
    /// request transfers the underlay to the compositor even when that race loses,
    /// so the buffer is released exactly once before the rejection is returned.
    pub(super) fn begin_move(&mut self, request: MoveBegin) -> io::Result<Option<io::Error>> {
        let underlay_buffer_id = *self
            .move_underlays
            .get(&request.surface_id)
            .ok_or_else(|| invalid("move underlay is not prepared"))?;
        let underlay = self
            .buffers
            .values
            .get_mut(&underlay_buffer_id)
            .ok_or_else(|| invalid("move underlay disappeared"))?;
        if underlay.owner != Owner::Desktop
            || underlay.busy
            || underlay.size != self.display
            || self.desktop_current_buffers.contains(&underlay_buffer_id)
        {
            return Err(invalid("move underlay buffer state invalid"));
        }
        underlay.busy = true;
        let rejection = if self.move_grab.is_some() {
            Some(invalid("move grab already active"))
        } else {
            None
        };
        let capture = self.pointer_capture.filter(|capture| {
            capture.surface_id == 0
                && capture.window_group == request.surface_id
                && capture.serial == request.serial
        });
        let rejection = rejection.or_else(|| {
            capture
                .is_none()
                .then(|| invalid("move begin is not authorized by pointer-down"))
        });
        let frame = self.presented_nodes.iter().find_map(|node| {
            (node.window_group == request.surface_id
                && node.kind == display_proto::SceneNodeKind::DisplayList)
                .then_some(node.clip)
        });
        let rejection = rejection.or_else(|| {
            frame
                .is_none()
                .then(|| invalid("move group is not presented"))
        });
        if let Some(error) = rejection {
            underlay.busy = false;
            return Ok(Some(error));
        }
        let capture = capture.expect("validated move capture");
        let frame = frame.expect("validated move frame");
        self.move_grab = Some(MoveGrab {
            surface_id: request.surface_id,
            underlay_buffer_id,
            down: capture.down,
            origin: (frame.x, frame.y),
            offset: (0, 0),
            limits: (request.min_x, request.min_y, request.max_x, request.max_y),
            ending: false,
        });
        Ok(None)
    }

    /// Returns and clears damage accumulated by compositor-side move updates.
    pub fn take_move_damage(&mut self) -> Option<Rect> {
        self.move_damage.take()
    }

    /// Returns the active move group and its current physical translation.
    pub fn active_move(&self) -> Option<(u32, (i32, i32), u32)> {
        self.move_grab
            .map(|grab| (grab.surface_id, grab.offset, grab.underlay_buffer_id))
    }

    /// Returns the transform a newly accepted scene still needs.
    pub fn scene_move(&self, scene: &super::Scene) -> Option<(u32, (i32, i32), u32)> {
        let grab = self.move_grab?;
        (!scene.finishes_move).then_some((grab.surface_id, grab.offset, grab.underlay_buffer_id))
    }

    /// Returns the last page-flip-complete flat scene used by move damage composition.
    pub fn presented_nodes(&self) -> &[super::scene::Node] {
        &self.presented_nodes
    }

    fn update_move(&mut self, x: i32, y: i32) {
        let Some(mut grab) = self.move_grab else {
            return;
        };
        let Some(next) = next_move_offset(grab, x, y) else {
            return;
        };
        let old = grab.offset;
        grab.offset = next;
        if grab.offset != old {
            if let Some(damage) = group_damage(&self.presented_nodes, grab.surface_id, old) {
                accumulate(&mut self.move_damage, damage);
            }
            if let Some(damage) = group_damage(&self.presented_nodes, grab.surface_id, grab.offset)
            {
                accumulate(&mut self.move_damage, damage);
            }
        }
        self.move_grab = Some(grab);
    }

    fn finish_move(&mut self) -> io::Result<()> {
        let Some(mut grab) = self.move_grab else {
            return Ok(());
        };
        let scale = display_proto::DEVICE_SCALE_FACTOR as i32;
        let message = MoveComplete {
            surface_id: grab.surface_id,
            x: (grab.origin.0 + grab.offset.0) / scale,
            y: (grab.origin.1 + grab.offset.1) / scale,
        };
        let mut bytes = [0u8; 32];
        let encoded = message
            .encode(&mut bytes)
            .ok_or_else(|| io::Error::other("move-complete encoding failed"))?;
        send_message(self.desktop_stream()?, encoded)?;
        grab.ending = true;
        self.move_grab = Some(grab);
        Ok(())
    }

    /// Routes one keyboard transition: while an accelerator grab owns the key
    /// stream every event (including the chord's own modifier transitions)
    /// goes to the desktop, otherwise the event goes to the presented focused
    /// surface. `modifier_keys` carries the physical modifier codes currently
    /// held so a matched chord grabs exactly the keys the user pressed.
    pub fn route_key(
        &mut self,
        code: u32,
        value: i32,
        modifiers: u32,
        modifier_keys: &[u16],
    ) -> io::Result<()> {
        let surface_id = match self
            .accelerators
            .route(code, value, modifiers, modifier_keys)
        {
            accelerator::KeyRoute::Desktop => 0,
            accelerator::KeyRoute::Focused => self.focused_surface,
        };
        let event = InputKey {
            surface_id,
            code,
            value,
            modifiers,
        };
        let mut bytes = [0u8; 40];
        let message = event
            .encode(&mut bytes)
            .ok_or_else(|| io::Error::other("key encoding failed"))?;
        send_message(self.target_stream(surface_id)?, message)
    }

    fn target_stream(&self, surface_id: u32) -> io::Result<&UnixStream> {
        if surface_id == 0 {
            self.desktop_stream()
        } else {
            self.apps
                .get(&surface_id)
                .map(|app| &app.stream)
                .ok_or_else(|| invalid("input target disappeared"))
        }
    }
}

fn next_move_offset(grab: MoveGrab, x: i32, y: i32) -> Option<(i32, i32)> {
    // `ending` is irreversible: MoveComplete already fixed the canonical
    // destination. Letting button-free motion mutate it makes the final scene
    // miss that destination, so the grab never retires and the window keeps
    // following the pointer after release.
    if grab.ending {
        return None;
    }
    let scale = display_proto::DEVICE_SCALE_FACTOR as i32;
    let x = (grab.origin.0 / scale + (x - grab.down.0) / scale).clamp(grab.limits.0, grab.limits.2);
    let y = (grab.origin.1 / scale + (y - grab.down.1) / scale).clamp(grab.limits.1, grab.limits.3);
    Some((x * scale - grab.origin.0, y * scale - grab.origin.1))
}

fn group_damage(
    nodes: &[super::scene::Node],
    window_group: u32,
    offset: (i32, i32),
) -> Option<Rect> {
    nodes
        .iter()
        .filter(|node| node.window_group == window_group)
        .filter_map(|node| intersect(node.bounds, node.clip))
        .map(|rectangle| translated(rectangle, offset))
        .reduce(union)
}

fn accumulate(total: &mut Option<Rect>, damage: Rect) {
    *total = Some(total.map_or(damage, |current| union(current, damage)));
}

fn intersect(left: Rect, right: Rect) -> Option<Rect> {
    let x1 = left.x.max(right.x);
    let y1 = left.y.max(right.y);
    let x2 = left
        .x
        .saturating_add_unsigned(left.width)
        .min(right.x.saturating_add_unsigned(right.width));
    let y2 = left
        .y
        .saturating_add_unsigned(left.height)
        .min(right.y.saturating_add_unsigned(right.height));
    (x2 > x1 && y2 > y1).then_some(Rect {
        x: x1,
        y: y1,
        width: (x2 - x1) as u32,
        height: (y2 - y1) as u32,
    })
}

fn translated(rectangle: Rect, offset: (i32, i32)) -> Rect {
    Rect {
        x: rectangle.x.saturating_add(offset.0),
        y: rectangle.y.saturating_add(offset.1),
        ..rectangle
    }
}

fn union(left: Rect, right: Rect) -> Rect {
    let x1 = left.x.min(right.x);
    let y1 = left.y.min(right.y);
    let x2 = left
        .x
        .saturating_add_unsigned(left.width)
        .max(right.x.saturating_add_unsigned(right.width));
    let y2 = left
        .y
        .saturating_add_unsigned(left.height)
        .max(right.y.saturating_add_unsigned(right.height));
    Rect {
        x: x1,
        y: y1,
        width: x2.saturating_sub(x1) as u32,
        height: y2.saturating_sub(y1) as u32,
    }
}

fn contains(rectangle: Rect, x: i32, y: i32) -> bool {
    x >= rectangle.x
        && y >= rectangle.y
        && x < rectangle.x.saturating_add_unsigned(rectangle.width)
        && y < rectangle.y.saturating_add_unsigned(rectangle.height)
}

#[cfg(test)]
mod tests {
    use super::{MoveGrab, next_move_offset};

    fn grab(ending: bool) -> MoveGrab {
        MoveGrab {
            surface_id: 1,
            underlay_buffer_id: 2,
            down: (100, 80),
            origin: (200, 120),
            offset: (0, 0),
            limits: (0, 0, 1000, 800),
            ending,
        }
    }

    #[test]
    fn active_move_tracks_pointer_in_physical_pixels() {
        assert_eq!(next_move_offset(grab(false), 140, 100), Some((40, 20)));
    }

    #[test]
    fn ending_move_ignores_button_free_motion() {
        assert_eq!(next_move_offset(grab(true), 400, 300), None);
    }
}
