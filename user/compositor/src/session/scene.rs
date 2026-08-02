//! Desktop scene commit validation and page-flip retirement.
//!
//! Owns the accepted-scene lifecycle: a `SceneCommit` is validated against
//! compositor-owned buffers and per-app readiness to produce a composable
//! [`Scene`], and each presented scene retires its buffers on exact flip
//! completion. Connection, handshake and per-message routing stay in the
//! parent module; this seam only knows how a frame becomes presentable.

use std::io::{self, Write as _};

use display_proto::{ClipMask, Rect, SceneCommit, SceneNodeKind};
use linux_uapi::drm::FlipEvent;

use super::buffers::Owner;
use super::wire::{send_accepted, send_discarded, send_presented};
use super::{RoutingNode, Session, invalid};

pub(super) fn app_first_scene_presented_marker(surface_id: u32) -> String {
    format!("compositor: app {surface_id} first scene presented\n")
}

/// One accepted flat-scene pixel layer.
#[derive(Clone)]
pub struct Node {
    pub kind: SceneNodeKind,
    pub window_group: u32,
    pub buffer_id: u32,
    pub bounds: Rect,
    pub clip: Rect,
    /// Rounded CSS masks applied inside the coarse rectangular clip.
    pub clip_masks: Vec<ClipMask>,
}

#[derive(Clone)]
struct AppPresentation {
    surface_id: u32,
    revision: u64,
    previous: Option<super::Content>,
}

/// Complete accepted desktop scene awaiting page-flip completion.
pub struct Scene {
    pub revision: u64,
    pub output_size: display_proto::Size,
    pub nodes: Vec<Node>,
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
        if commit.output_serial != self.output_serial {
            return self.discard_commit(commit);
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
            // The composited bounds default to the declared node size; the
            // foreign-surface fallback may clamp them to a stale buffer's real
            // size so composite_node never reads past the source.
            let mut node_bounds = node.bounds;
            let buffer_id = match node.kind {
                SceneNodeKind::DisplayList => {
                    if node.source_id != 0 {
                        return Err(invalid("desktop display-list source identity invalid"));
                    }
                    let render_id = self
                        .desktop_render_id
                        .ok_or_else(|| invalid("desktop display list is not rendered"))?;
                    let buffer = self
                        .buffers
                        .values
                        .get(&render_id)
                        .ok_or_else(|| invalid("desktop render target disappeared"))?;
                    if buffer.owner != Owner::Desktop
                        || buffer.busy && !self.desktop_current_buffers.contains(&render_id)
                        || buffer.size.width != node.bounds.width
                        || buffer.size.height != node.bounds.height
                    {
                        return Err(invalid("desktop buffer state invalid"));
                    }
                    if !desktop_buffers.contains(&render_id) {
                        desktop_buffers.push(render_id);
                    }
                    render_id
                }
                SceneNodeKind::ForeignSurface => {
                    let app = self
                        .apps
                        .get(&node.source_id)
                        .expect("validated foreign surface");
                    // Prefer the exact-serial content (pending, else current),
                    // which matches the node bounds and adopts cleanly. If
                    // neither matches the node's serial — a normal in-flight
                    // configure handshake mid resize/maximize — fall back to the
                    // app's last-presented `current` content of ANY serial rather
                    // than skipping the node. Skipping leaves the region cleared
                    // to black for that frame; with the app committing a new
                    // serial almost every frame during a rapid resize, that
                    // produced a sustained black flicker. Showing slightly-stale
                    // (older-serial/older-size) content for a frame is
                    // imperceptible; a black flash is not.
                    let exact = app
                        .pending
                        .as_ref()
                        .filter(|content| content.configure_serial == node.configure_serial)
                        .or_else(|| {
                            app.current
                                .as_ref()
                                .filter(|content| content.configure_serial == node.configure_serial)
                        });
                    let (content, exact_match) = match exact {
                        Some(content) => (content, true),
                        // Fall back to ANY content the app holds, newest-first:
                        // prefer `current` (already presented) but accept
                        // `pending` (staged, its buffer is validated) when there
                        // is no current yet — during a rapid resize the app's
                        // only content is often a pending buffer at a serial the
                        // node hasn't caught up to. Showing it a frame early/late
                        // beats clearing to black.
                        None => match app.current.as_ref().or(app.pending.as_ref()) {
                            Some(content) => (content, false),
                            None => {
                                // The app has committed NO buffer at all yet
                                // (first-frame handshake): nothing to show, and no
                                // prior frame to flicker against, so skip.
                                eprintln!(
                                    "compositor: foreign surface {} serial {} not ready, skipped",
                                    node.source_id, node.configure_serial
                                );
                                continue;
                            }
                        },
                    };
                    let buffer = &self.buffers.values[&content.buffer_id];
                    // The node bounds drive composite_node's source indexing, so
                    // they MUST equal the content buffer's real size or the copy
                    // reads out of bounds. On the exact-serial path they already
                    // agree (the desktop laid the node out for this size). On the
                    // fallback path the stale buffer can differ, so clamp the
                    // node's bounds to the buffer size (keeping the origin); the
                    // uncovered edge shows the desktop-owned window body, never a
                    // black clear.
                    let mut effective_bounds = node.bounds;
                    if buffer.size.width != node.bounds.width
                        || buffer.size.height != node.bounds.height
                    {
                        if exact_match {
                            // Exact serial but wrong geometry is a transient
                            // layout/handshake disagreement; keep the previous
                            // skip semantics (self-heals next commit).
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
                        effective_bounds.width = buffer.size.width;
                        effective_bounds.height = buffer.size.height;
                    }
                    node_bounds = effective_bounds;
                    if exact_match
                        && app
                            .pending
                            .as_ref()
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
                    SceneNodeKind::DisplayList => 0,
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
                bounds: node_bounds,
                clip: node.clip,
                clip_masks: node.clip_masks.iter().collect(),
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
            let previous = app.current.replace(next);
            app_presentations.push(AppPresentation {
                surface_id,
                revision,
                previous,
            });
        }
        let desktop = self.desktop.as_mut().expect("validated desktop");
        desktop.last_revision = commit.revision;
        send_accepted(&desktop.stream, commit.revision)?;
        let finishes_move = self.move_grab.is_some_and(|grab| {
            grab.ending
                && nodes.iter().any(|node| {
                    node.kind == SceneNodeKind::DisplayList
                        && node.window_group == grab.surface_id
                        && node.clip.x == grab.origin.0 + grab.offset.0
                        && node.clip.y == grab.origin.1 + grab.offset.1
                })
        });
        Ok(Scene {
            revision: commit.revision,
            output_size: self.display,
            nodes,
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
            self.buffers.values.remove(&id);
        }
        self.desktop_current_buffers
            .clone_from(&scene.desktop_buffers);
        send_presented(&desktop.stream, scene.revision, event)?;
        for app_use in &scene.app_presentations {
            if let Some(app) = self.apps.get_mut(&app_use.surface_id) {
                if let Some(previous) = &app_use.previous {
                    self.buffers.values.remove(&previous.buffer_id);
                }
                send_presented(&app.stream, app_use.revision, event)?;
                let first_scene_presented =
                    !std::mem::replace(&mut app.first_scene_presented, true);
                if first_scene_presented {
                    let marker = app_first_scene_presented_marker(app_use.surface_id);
                    // One preformatted write keeps concurrent app diagnostics
                    // from splicing bytes into this runtime ordering barrier.
                    let _ = io::stderr().write_all(marker.as_bytes());
                }
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
            self.move_underlays.remove(&grab.surface_id);
            self.buffers.values.remove(&grab.underlay_buffer_id);
            self.move_damage = None;
        }
        if !self.first_scene_presented {
            self.first_scene_presented = true;
            eprintln!("compositor: desktop first scene presented");
        }
        Ok(())
    }

    fn discard_commit(&mut self, commit: SceneCommit<'_>) -> io::Result<Scene> {
        if !commit
            .nodes()
            .any(|node| node.kind == SceneNodeKind::DisplayList && node.source_id == 0)
        {
            return Err(invalid("discarded desktop scene has no pixels"));
        }
        let desktop = self.desktop.as_mut().expect("validated desktop");
        desktop.last_revision = commit.revision;
        send_discarded(&desktop.stream, commit.revision)?;
        Ok(Scene::discarded(commit.revision))
    }

    /// Terminates an already validated scene when the connector changed again
    /// between socket validation and the KMS modeset.
    pub fn discard_scene(&mut self, scene: &Scene) -> io::Result<()> {
        let desktop = self
            .desktop
            .as_ref()
            .ok_or_else(|| invalid("desktop disappeared"))?;
        send_discarded(&desktop.stream, scene.revision)?;
        for id in &scene.desktop_buffers {
            let stale = self
                .buffers
                .values
                .get(id)
                .is_some_and(|buffer| buffer.size != self.display);
            if stale {
                self.buffers.values.remove(id);
            } else {
                self.buffers
                    .values
                    .get_mut(id)
                    .ok_or_else(|| invalid("discarded GPU target disappeared"))?
                    .busy = false;
            }
        }
        for presentation in &scene.app_presentations {
            let app = self
                .apps
                .get_mut(&presentation.surface_id)
                .ok_or_else(|| invalid("discarded app disappeared"))?;
            let adopted = app
                .current
                .take()
                .ok_or_else(|| invalid("discarded app adoption missing"))?;
            app.current = presentation.previous.clone();
            app.pending = Some(adopted);
        }
        Ok(())
    }
}

impl Scene {
    pub(crate) fn window_groups(&self) -> impl Iterator<Item = u32> + '_ {
        self.nodes
            .iter()
            .map(|node| node.window_group)
            .filter(|group| *group != 0)
    }

    fn discarded(revision: u64) -> Self {
        Self {
            revision,
            output_size: display_proto::Size::default(),
            nodes: Vec::new(),
            finishes_move: false,
            desktop_buffers: Vec::new(),
            app_presentations: Vec::new(),
            routing: Vec::new(),
            focused_surface: 0,
        }
    }

    /// Reports that this is a terminal protocol acknowledgement, not a scene.
    pub fn is_discarded(&self) -> bool {
        self.nodes.is_empty()
    }
}
