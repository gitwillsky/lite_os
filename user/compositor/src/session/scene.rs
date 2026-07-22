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
pub struct Node {
    pub buffer_id: u32,
    pub bounds: Rect,
    pub clip: Rect,
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
        if commit.revision <= last_revision
            || (commit.focused_surface != 0 && !self.apps.contains_key(&commit.focused_surface))
        {
            return Err(invalid("scene revision or focus invalid"));
        }
        let mut nodes = Vec::with_capacity(commit.nodes().len());
        let mut desktop_buffers = Vec::new();
        let mut adoptions = Vec::new();
        let mut routing = Vec::new();
        for node in commit.nodes() {
            let buffer_id = match node.kind {
                SceneNodeKind::Pixels => {
                    let buffer = self
                        .buffers
                        .values
                        .get(&node.source_id)
                        .ok_or_else(|| invalid("unknown desktop buffer"))?;
                    if buffer.owner != Owner::Desktop
                        || buffer.busy
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
                        .ok_or_else(|| invalid("unknown foreign surface"))?;
                    let content = app
                        .pending
                        .filter(|content| content.configure_serial == node.configure_serial)
                        .or_else(|| {
                            app.current
                                .filter(|content| content.configure_serial == node.configure_serial)
                        })
                        .ok_or_else(|| invalid("foreign surface is not ready"))?;
                    let buffer = &self.buffers.values[&content.buffer_id];
                    if buffer.size.width != node.bounds.width
                        || buffer.size.height != node.bounds.height
                    {
                        return Err(invalid("foreign surface geometry mismatch"));
                    }
                    if app
                        .pending
                        .is_some_and(|pending| pending.buffer_id == content.buffer_id)
                        && !adoptions.contains(&node.source_id)
                    {
                        adoptions.push(node.source_id);
                    }
                    content.buffer_id
                }
            };
            routing.push(RoutingNode {
                surface_id: match node.kind {
                    SceneNodeKind::Pixels => 0,
                    SceneNodeKind::ForeignSurface => node.source_id,
                },
                bounds: node.bounds,
                input: node.input.iter().collect(),
            });
            nodes.push(Node {
                buffer_id,
                bounds: node.bounds,
                clip: node.clip,
            });
        }
        if nodes.is_empty() {
            return Err(invalid("desktop scene is empty"));
        }
        for id in &desktop_buffers {
            self.buffers
                .values
                .get_mut(id)
                .expect("validated desktop buffer")
                .busy = true;
        }
        let mut app_presentations = Vec::new();
        for surface_id in adoptions {
            let app = self
                .apps
                .get_mut(&surface_id)
                .expect("validated app adoption");
            let next = app.pending.take().expect("adopted pending content");
            let previous_buffer = app.current.replace(next).map(|content| content.buffer_id);
            app_presentations.push(AppPresentation {
                surface_id,
                revision: next.revision,
                previous_buffer,
            });
        }
        let desktop = self.desktop.as_mut().expect("validated desktop");
        desktop.last_revision = commit.revision;
        send_accepted(&desktop.stream, commit.revision)?;
        Ok(Scene {
            revision: commit.revision,
            nodes,
            desktop_buffers,
            app_presentations,
            routing,
            focused_surface: commit.focused_surface,
        })
    }

    /// Releases presentation-retired buffers and publishes exact flip completion.
    pub fn presented(&mut self, scene: &Scene, event: FlipEvent) -> io::Result<()> {
        let desktop = self
            .desktop
            .as_ref()
            .ok_or_else(|| io::Error::other("desktop disappeared"))?;
        for id in &scene.desktop_buffers {
            release_buffer(&mut self.buffers, &desktop.stream, *id)?;
        }
        send_presented(&desktop.stream, scene.revision, event)?;
        for app_use in &scene.app_presentations {
            if let Some(app) = self.apps.get(&app_use.surface_id) {
                if let Some(previous) = app_use.previous_buffer {
                    release_buffer(&mut self.buffers, &app.stream, previous)?;
                }
                send_presented(&app.stream, app_use.revision, event)?;
            }
        }
        self.routing.clone_from(&scene.routing);
        self.focused_surface = scene.focused_surface;
        if !self.first_scene_presented {
            self.first_scene_presented = true;
            eprintln!("compositor: desktop first scene presented");
        }
        Ok(())
    }
}

fn release_buffer(buffers: &mut Buffers, stream: &UnixStream, id: u32) -> io::Result<()> {
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
