//! Desktop scene commit validation and page-flip retirement.
//!
//! Owns the accepted-scene lifecycle: a `SceneCommit` is validated against
//! compositor-owned buffers and per-app readiness to produce a composable
//! [`Scene`], and each presented scene retires its buffers on exact flip
//! completion. Connection, handshake and per-message routing stay in the
//! parent module; this seam only knows how a frame becomes presentable.

use std::{io, os::unix::net::UnixStream};

use display_proto::{BufferRelease, Rect, SceneCommit, SceneNodeKind, send_message};
use linux_uapi::drm::FlipEvent;

use super::buffers::{Buffers, Owner};
use super::wire::{send_accepted, send_presented};
use super::{RoutingNode, Session, invalid};

/// One accepted flat-scene pixel layer.
#[derive(Clone)]
pub struct Node {
    pub kind: SceneNodeKind,
    pub window_group: u32,
    pub buffer_id: u32,
    pub bounds: Rect,
    pub clip: Rect,
    pub opaque: Option<Rect>,
    pub damage: Vec<Rect>,
    /// Rounded-corner radius in physical pixels; the compositor skips corner
    /// pixels outside the arc so lower content shows through the frame clip.
    pub corner_radius: u32,
}

#[derive(Clone, Copy)]
struct AppPresentation {
    surface_id: u32,
    revision: u64,
    previous_buffer: Option<u32>,
}

/// Complete accepted desktop scene awaiting page-flip completion.
pub struct Scene {
    pub revision: u64,
    pub nodes: Vec<Node>,
    pub damage: Rect,
    pub(super) finishes_move: bool,
    desktop_buffers: Vec<u32>,
    app_presentations: Vec<AppPresentation>,
    routing: Vec<RoutingNode>,
    focused_surface: u32,
}

impl Session {
    pub(super) fn accept_scene(&mut self, payload: &[u8]) -> io::Result<Scene> {
        let commit = SceneCommit::parse(payload).ok_or_else(|| invalid("invalid scene"))?;
        let last_revision = self
            .desktop
            .as_ref()
            .ok_or_else(|| invalid("desktop disappeared"))?
            .last_revision;
        if commit.revision <= last_revision {
            return Err(invalid("scene revision invalid"));
        }
        // App disconnect always races the desktop's next commit: the compositor
        // removes a dead app before AppClosed reaches the desktop, so focus or
        // a foreign node may still reference it. That reference can only be
        // explained by the race, so clamp/skip it; every later violation
        // (unknown buffer, bad geometry, stale serial) stays epoch-fatal.
        let focused_surface =
            if commit.focused_surface != 0 && !self.apps.contains_key(&commit.focused_surface) {
                eprintln!(
                    "compositor: focus {} clamped to desktop after app disconnect",
                    commit.focused_surface
                );
                0
            } else {
                commit.focused_surface
            };
        let mut nodes = Vec::with_capacity(commit.nodes().len());
        let mut desktop_buffers = Vec::new();
        let mut adoptions = Vec::new();
        let mut routing = Vec::new();
        for node in commit.nodes() {
            if node.kind == SceneNodeKind::ForeignSurface
                && !self.apps.contains_key(&node.source_id)
            {
                eprintln!(
                    "compositor: foreign surface {} skipped after app disconnect",
                    node.source_id
                );
                continue;
            }
            let mut damage: Vec<Rect> = node.damage.iter().collect();
            let buffer_id = match node.kind {
                SceneNodeKind::Pixels => {
                    let buffer = self
                        .buffers
                        .values
                        .get(&node.source_id)
                        .ok_or_else(|| invalid("unknown desktop buffer"))?;
                    if buffer.owner != Owner::Desktop
                        || buffer.busy && !self.desktop_current_buffers.contains(&node.source_id)
                        || buffer.size.width != node.bounds.width
                        || buffer.size.height != node.bounds.height
                    {
                        return Err(invalid("desktop buffer state invalid"));
                    }
                    if !desktop_buffers.contains(&node.source_id) {
                        desktop_buffers.push(node.source_id);
                    }
                    node.source_id
                }
                SceneNodeKind::ForeignSurface => {
                    let app = self
                        .apps
                        .get(&node.source_id)
                        .expect("validated foreign surface");
                    // The desktop bakes a node's `configure_serial` and `bounds`
                    // from independent React state at render time, and its
                    // `ready` set is never pruned, so a scene can legitimately
                    // reference a serial the app has already superseded (its
                    // buffer recycled) or one whose buffer no longer matches the
                    // freshly-laid-out bounds. That is a normal configure
                    // handshake in flight — mid maximize/restore/resize — not a
                    // protocol violation. Skip the node this frame (like the
                    // app-disconnect races above) and let the desktop re-emit it
                    // once app and layout agree; killing the epoch here is what
                    // dropped the whole desktop to the splash on maximize.
                    let content = app
                        .pending
                        .as_ref()
                        .filter(|content| content.configure_serial == node.configure_serial)
                        .or_else(|| {
                            app.current
                                .as_ref()
                                .filter(|content| content.configure_serial == node.configure_serial)
                        });
                    let Some(content) = content else {
                        eprintln!(
                            "compositor: foreign surface {} serial {} not ready, skipped",
                            node.source_id, node.configure_serial
                        );
                        continue;
                    };
                    let buffer = &self.buffers.values[&content.buffer_id];
                    if buffer.size.width != node.bounds.width
                        || buffer.size.height != node.bounds.height
                    {
                        eprintln!(
                            "compositor: foreign surface {} geometry {}x{} != node {}x{}, skipped",
                            node.source_id,
                            buffer.size.width,
                            buffer.size.height,
                            node.bounds.width,
                            node.bounds.height
                        );
                        continue;
                    }
                    if app
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.buffer_id == content.buffer_id)
                        && !adoptions.contains(&node.source_id)
                    {
                        adoptions.push(node.source_id);
                        damage.extend(content.damage.iter().map(|rectangle| Rect {
                            x: node.bounds.x.saturating_add(rectangle.x),
                            y: node.bounds.y.saturating_add(rectangle.y),
                            width: rectangle.width,
                            height: rectangle.height,
                        }));
                    }
                    content.buffer_id
                }
            };
            routing.push(RoutingNode {
                surface_id: match node.kind {
                    SceneNodeKind::Pixels => 0,
                    SceneNodeKind::ForeignSurface => node.source_id,
                },
                window_group: node.window_group,
                bounds: node.bounds,
                input: node.input.iter().collect(),
            });
            nodes.push(Node {
                kind: node.kind,
                window_group: node.window_group,
                buffer_id,
                bounds: node.bounds,
                clip: node.clip,
                opaque: node.opaque,
                damage,
                corner_radius: node.corner_radius,
            });
        }
        if nodes.is_empty() {
            return Err(invalid("desktop scene is empty"));
        }
        for id in &desktop_buffers {
            let buffer = self
                .buffers
                .values
                .get_mut(id)
                .expect("validated desktop buffer");
            buffer.busy = true;
        }
        let mut app_presentations = Vec::new();
        for surface_id in adoptions {
            let app = self
                .apps
                .get_mut(&surface_id)
                .expect("validated app adoption");
            let next = app.pending.take().expect("adopted pending content");
            let revision = next.revision;
            let previous_buffer = app.current.replace(next).map(|content| content.buffer_id);
            app_presentations.push(AppPresentation {
                surface_id,
                revision,
                previous_buffer,
            });
        }
        let desktop = self.desktop.as_mut().expect("validated desktop");
        desktop.last_revision = commit.revision;
        send_accepted(&desktop.stream, commit.revision)?;
        let full = Rect {
            x: 0,
            y: 0,
            width: self.display.width,
            height: self.display.height,
        };
        let damage = if self.presented_nodes.is_empty() {
            full
        } else {
            let pixels = nodes
                .iter()
                .flat_map(|node| node.damage.iter().copied())
                .filter_map(|rectangle| intersect(rectangle, full))
                .reduce(union);
            geometry_damage(&self.presented_nodes, &nodes)
                .filter_map(|rectangle| intersect(rectangle, full))
                .fold(pixels, |total, rectangle| {
                    Some(total.map_or(rectangle, |current| union(current, rectangle)))
                })
                .unwrap_or(full)
        };
        let finishes_move = self.move_grab.is_some_and(|grab| {
            grab.ending
                && nodes.iter().any(|node| {
                    node.kind == SceneNodeKind::Pixels
                        && node.window_group == grab.surface_id
                        && node.clip.x == grab.origin.0 + grab.offset.0
                        && node.clip.y == grab.origin.1 + grab.offset.1
                })
        });
        Ok(Scene {
            revision: commit.revision,
            nodes,
            damage,
            finishes_move,
            desktop_buffers,
            app_presentations,
            routing,
            focused_surface,
        })
    }

    /// Releases presentation-retired buffers and publishes exact flip completion.
    pub fn presented(&mut self, scene: &Scene, event: FlipEvent) -> io::Result<()> {
        self.last_flip = event;
        let desktop = self
            .desktop
            .as_ref()
            .ok_or_else(|| io::Error::other("desktop disappeared"))?;
        let retired: Vec<_> = self
            .desktop_current_buffers
            .iter()
            .copied()
            .filter(|id| !scene.desktop_buffers.contains(id))
            .collect();
        for id in retired {
            release_buffer(&mut self.buffers, &desktop.stream, id)?;
        }
        self.desktop_current_buffers
            .clone_from(&scene.desktop_buffers);
        send_presented(&desktop.stream, scene.revision, event)?;
        for app_use in &scene.app_presentations {
            if let Some(app) = self.apps.get(&app_use.surface_id) {
                if let Some(previous) = app_use.previous_buffer {
                    // A previous buffer sized for a superseded configure can
                    // never be presented again: retire it instead of recycling.
                    // The matching release path keeps double buffering intact.
                    let stale = app.configure.is_some_and(|configure| {
                        let buffer = self
                            .buffers
                            .values
                            .get(&previous)
                            .expect("presented app buffer");
                        buffer.size.width != configure.width * display_proto::DEVICE_SCALE_FACTOR
                            || buffer.size.height
                                != configure.height * display_proto::DEVICE_SCALE_FACTOR
                    });
                    if stale {
                        self.buffers.values.remove(&previous);
                        let mut bytes = [0u8; 24];
                        let message = BufferRelease {
                            buffer_id: previous,
                        }
                        .encode(&mut bytes)
                        .ok_or_else(|| io::Error::other("release encoding failed"))?;
                        send_message(&app.stream, message)?;
                    } else {
                        release_buffer(&mut self.buffers, &app.stream, previous)?;
                    }
                }
                send_presented(&app.stream, app_use.revision, event)?;
            }
        }
        self.routing.clone_from(&scene.routing);
        self.presented_nodes.clone_from(&scene.nodes);
        self.focused_surface = scene.focused_surface;
        if scene.finishes_move {
            let grab = self
                .move_grab
                .take()
                .expect("move-finishing scene requires an active grab");
            release_buffer(&mut self.buffers, &desktop.stream, grab.underlay_buffer_id)?;
            self.move_damage = None;
        }
        if !self.first_scene_presented {
            self.first_scene_presented = true;
            eprintln!("compositor: desktop first scene presented");
        }
        Ok(())
    }
}

fn geometry_damage(previous: &[Node], current: &[Node]) -> impl Iterator<Item = Rect> {
    let mut damage = [None; 2];
    let count = previous.len().max(current.len());
    (0..count).flat_map(move |index| {
        damage.fill(None);
        match (previous.get(index), current.get(index)) {
            (Some(old), Some(new))
                if old.kind == new.kind
                    && old.window_group == new.window_group
                    && old.bounds == new.bounds
                    && old.clip == new.clip => {}
            (Some(old), Some(new)) => {
                damage[0] = intersect(old.bounds, old.clip);
                damage[1] = intersect(new.bounds, new.clip);
            }
            (Some(old), None) => {
                damage[0] = intersect(old.bounds, old.clip);
            }
            (None, Some(new)) => {
                damage[0] = intersect(new.bounds, new.clip);
            }
            (None, None) => unreachable!(),
        }
        damage.into_iter().flatten()
    })
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

pub(super) fn release_buffer(
    buffers: &mut Buffers,
    stream: &UnixStream,
    id: u32,
) -> io::Result<()> {
    buffers
        .values
        .get_mut(&id)
        .ok_or_else(|| invalid("released buffer disappeared"))?
        .busy = false;
    let mut bytes = [0u8; 24];
    let message = BufferRelease { buffer_id: id }
        .encode(&mut bytes)
        .ok_or_else(|| io::Error::other("release encoding failed"))?;
    send_message(stream, message)
}
