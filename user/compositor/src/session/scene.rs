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
use super::{Content, MoveGrab, RoutingNode, Session, invalid};

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
    /// Conservative physical region changed from the preceding accepted scene.
    pub damage: Rect,
    pub nodes: Vec<Node>,
    pub(super) finishes_move: bool,
    desktop_buffers: Vec<u32>,
    app_presentations: Vec<AppPresentation>,
    routing: Vec<RoutingNode>,
    focused_surface: u32,
}

/// How an accepted scene relates to the compositor-owned move transaction.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum SceneMove {
    Finished,
    Active((u32, (i32, i32), u32)),
    MissingGroup(u32),
}

impl Scene {
    /// Projects one active grab onto this accepted scene.
    ///
    /// # Parameters
    ///
    /// - `grab`: The compositor-owned move transaction awaiting presentation.
    ///
    /// # Returns
    ///
    /// Returns whether the canonical scene finishes the move, still needs its
    /// temporary transform, or no longer contains the transaction's window.
    pub(super) fn project_move(&self, grab: MoveGrab) -> SceneMove {
        if self.finishes_move {
            SceneMove::Finished
        } else if self
            .nodes
            .iter()
            .all(|node| node.window_group != grab.surface_id)
        {
            SceneMove::MissingGroup(grab.surface_id)
        } else {
            SceneMove::Active((grab.surface_id, grab.offset, grab.underlay_buffer_id))
        }
    }
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
        // Recoverable disconnect race — see `Session::app_is_live`. Focus or a
        // foreign node may still reference an app the compositor just dropped;
        // clamp/skip it. Every later violation (unknown buffer, bad geometry,
        // stale serial) stays epoch-fatal.
        let focused_surface =
            if commit.focused_surface != 0 && !self.app_is_live(commit.focused_surface) {
                Self::log_surface_race("scene focus", commit.focused_surface);
                0
            } else {
                commit.focused_surface
            };
        let mut nodes = Vec::with_capacity(commit.nodes().len());
        let mut desktop_buffers = Vec::new();
        let mut adoptions = Vec::new();
        let mut routing = Vec::new();
        let output = Rect {
            x: 0,
            y: 0,
            width: self.display.width,
            height: self.display.height,
        };
        let mut damage = Rect::default();
        for node in commit.nodes() {
            if node.kind == SceneNodeKind::ForeignSurface && !self.app_is_live(node.source_id) {
                Self::log_surface_race("scene foreign surface", node.source_id);
                continue;
            }
            for changed in node.damage.iter() {
                if let Some(changed) = intersect_rect(changed, output) {
                    damage = union_rect(damage, changed);
                }
            }
            // The composited bounds default to the declared node size (correct
            // for display lists). The foreign-surface arm overrides them with
            // the chosen content's true buffer size so a held frame paints
            // unstretched.
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
                    let Some(app) = self.apps.get(&node.source_id) else {
                        // Independent of the earlier live-app skip's position —
                        // see fix 7. A dropped app here is the disconnect race.
                        Session::log_surface_race("scene foreign content", node.source_id);
                        continue;
                    };
                    // Two deterministic paths, no per-frame heuristic:
                    // Two deterministic paths, no per-frame heuristic:
                    //
                    // 1. EXACT: the app has drawn the node's requested serial
                    //    (pending preferred over current). Composite it; adopt
                    //    the pending buffer so the flip presents it.
                    // 2. HOLD-LAST: the app has not yet drawn this serial (the
                    //    normal in-flight configure handshake mid resize) — hold
                    //    the last-presented `current` frame. A held frame keeps
                    //    its OWN serial and OWN real size; it is never stretched
                    //    to the requested serial's geometry.
                    //
                    // Either way the composited bounds come from the chosen
                    // content's true buffer size (`choice.size`), so a held
                    // frame paints at its real size over the desktop-owned
                    // window body — never black-cleared, never geometry-guessed.
                    // Only when the app has committed no buffer at all (first
                    // frame) is there nothing to show, and the node is skipped.
                    let Some(choice) = select_foreign_content(
                        app.pending.as_ref(),
                        app.current.as_ref(),
                        node.configure_serial,
                    ) else {
                        eprintln!(
                            "compositor: foreign surface {} serial {} not ready, skipped",
                            node.source_id, node.configure_serial
                        );
                        continue;
                    };
                    // Composite at the content's true buffer size and the node's
                    // origin. The GPU sampler already sources from the real
                    // texture dims, so this only fixes the destination rect: a
                    // held (older-size) frame paints unstretched.
                    node_bounds = Rect {
                        x: node.bounds.x,
                        y: node.bounds.y,
                        width: choice.size.width,
                        height: choice.size.height,
                    };
                    if choice.adopt && !adoptions.contains(&node.source_id) {
                        adoptions.push(node.source_id);
                    }
                    choice.buffer_id
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
        // Token equality alone retires the grab — never pixel geometry. See
        // `move_grab_finishes`.
        let finishes_move = move_grab_finishes(self.move_grab, commit.move_token);
        Ok(Scene {
            revision: commit.revision,
            output_size: self.display,
            damage,
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
        if self.desktop.is_none() {
            return Err(io::Error::other("desktop disappeared"));
        }
        let retired: Vec<_> = self
            .desktop_current_buffers
            .iter()
            .copied()
            .filter(|id| !scene.desktop_buffers.contains(id))
            .collect();
        for id in retired {
            self.recycle_buffer(id);
        }
        self.desktop_current_buffers
            .clone_from(&scene.desktop_buffers);
        send_presented(
            &self.desktop.as_ref().expect("checked desktop").stream,
            scene.revision,
            event,
        )?;
        for app_use in &scene.app_presentations {
            if let Some(app) = self.apps.get_mut(&app_use.surface_id) {
                let retired = app_use.previous.as_ref().map(|content| content.buffer_id);
                send_presented(&app.stream, app_use.revision, event)?;
                let first_scene_presented =
                    !std::mem::replace(&mut app.first_scene_presented, true);
                if first_scene_presented {
                    let marker = app_first_scene_presented_marker(app_use.surface_id);
                    // One preformatted write keeps concurrent app diagnostics
                    // from splicing bytes into this runtime ordering barrier.
                    let _ = io::stderr().write_all(marker.as_bytes());
                }
                if let Some(id) = retired {
                    self.recycle_buffer(id);
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
            self.buffers.values.remove(&grab.underlay_buffer_id);
            self.move_changed = false;
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
    /// Revokes one disconnected app from an accepted but not yet presented scene.
    ///
    /// # Parameters
    ///
    /// - `surface_id`: The compositor-assigned app surface to remove.
    ///
    /// The scene remains presentable and expands damage over every removed node.
    pub(super) fn revoke_surface(&mut self, surface_id: u32) {
        let output = Rect {
            x: 0,
            y: 0,
            width: self.output_size.width,
            height: self.output_size.height,
        };
        for node in self
            .nodes
            .iter()
            .filter(|node| node.window_group == surface_id)
        {
            if let Some(removed) = intersect_rect(node.clip, output) {
                self.damage = union_rect(self.damage, removed);
            }
        }
        self.nodes.retain(|node| node.window_group != surface_id);
        super::revoke_surface_routing(&mut self.routing, surface_id);
        self.app_presentations
            .retain(|presentation| presentation.surface_id != surface_id);
        if self.focused_surface == surface_id {
            self.focused_surface = 0;
        }
    }

    fn discarded(revision: u64) -> Self {
        Self {
            revision,
            output_size: display_proto::Size::default(),
            damage: Rect::default(),
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

/// Whether an accepted scene finalizes the active move grab.
///
/// The grab retires only when it is `ending` (its `MoveComplete` was sent) and
/// the commit echoes its exact `move_token`. Geometry is never compared, so the
/// logical↔physical rounding round-trip can no longer strand the underlay
/// buffer by making a coordinate equality silently fail. A zero commit token
/// (no move finalizing) never matches a live grab's non-zero token.
fn move_grab_finishes(grab: Option<MoveGrab>, commit_token: u64) -> bool {
    grab.is_some_and(|grab| grab.ending && commit_token == grab.move_token)
}

/// The chosen foreign-surface content for one accepted scene node.
struct ForeignChoice {
    /// Compositor buffer to composite.
    buffer_id: u32,
    /// The buffer's true pixel size; the node is composited at this size so a
    /// held (older-serial) frame is never stretched to the requested geometry.
    size: display_proto::Size,
    /// Whether this is the app's exact-serial `pending` buffer and must be
    /// adopted (promoted `pending` → `current`) when the scene is presented.
    adopt: bool,
}

/// Picks which foreign-surface content one scene node composites, with no
/// per-frame heuristic — the two deterministic paths documented at the call
/// site (EXACT / HOLD-LAST).
///
/// # Parameters
///
/// - `pending`: The app's staged (accepted, not yet presented) content.
/// - `current`: The app's last-presented content.
/// - `serial`: The `configure_serial` the scene node requested.
///
/// # Returns
///
/// `None` only when the app has committed no buffer at all (first-frame
/// handshake): there is nothing to show and no prior pixels to flicker against,
/// so the caller skips the node.
fn select_foreign_content(
    pending: Option<&Content>,
    current: Option<&Content>,
    serial: u64,
) -> Option<ForeignChoice> {
    // EXACT (pending): the app drew the requested serial and it is staged —
    // composite and adopt it so the flip presents it.
    if let Some(pending) = pending.filter(|content| content.configure_serial == serial) {
        return Some(ForeignChoice {
            buffer_id: pending.buffer_id,
            size: pending.size,
            adopt: true,
        });
    }
    // EXACT (current): the requested serial is already presented — show it,
    // nothing to adopt.
    if let Some(current) = current.filter(|content| content.configure_serial == serial) {
        return Some(ForeignChoice {
            buffer_id: current.buffer_id,
            size: current.size,
            adopt: false,
        });
    }
    // HOLD-LAST: the requested serial is still in flight — hold the previous
    // presented frame verbatim (its own serial, its own real size).
    current.map(|current| ForeignChoice {
        buffer_id: current.buffer_id,
        size: current.size,
        adopt: false,
    })
}

fn intersect_rect(left: Rect, right: Rect) -> Option<Rect> {
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
        width: x2.saturating_sub(x1) as u32,
        height: y2.saturating_sub(y1) as u32,
    })
}

fn union_rect(left: Rect, right: Rect) -> Rect {
    if left.width == 0 || left.height == 0 {
        return right;
    }
    if right.width == 0 || right.height == 0 {
        return left;
    }
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

#[cfg(test)]
mod damage_tests {
    use super::{
        AppPresentation, Content, ForeignChoice, Node, Scene, SceneMove, intersect_rect,
        move_grab_finishes, select_foreign_content, union_rect,
    };
    use crate::session::{MoveGrab, RoutingNode};
    use display_proto::{Rect, SceneNodeKind, Size};

    #[test]
    fn scene_damage_is_clipped_to_output_then_conservatively_unioned() {
        let output = Rect {
            x: 0,
            y: 0,
            width: 300,
            height: 200,
        };
        let left = intersect_rect(
            Rect {
                x: -20,
                y: 30,
                width: 80,
                height: 50,
            },
            output,
        )
        .unwrap();
        let right = intersect_rect(
            Rect {
                x: 250,
                y: 10,
                width: 100,
                height: 30,
            },
            output,
        )
        .unwrap();
        assert_eq!(
            union_rect(left, right),
            Rect {
                x: 0,
                y: 10,
                width: 300,
                height: 70,
            }
        );
    }

    #[test]
    fn removed_surface_cancels_move_projection_in_unpresented_scene() {
        let mut scene = Scene {
            revision: 9,
            output_size: Size {
                width: 300,
                height: 200,
            },
            damage: Rect::default(),
            nodes: vec![
                Node {
                    kind: SceneNodeKind::DisplayList,
                    window_group: 0,
                    buffer_id: 1,
                    bounds: Rect::default(),
                    clip: Rect::default(),
                    clip_masks: Vec::new(),
                },
                Node {
                    kind: SceneNodeKind::ForeignSurface,
                    window_group: 7,
                    buffer_id: 2,
                    bounds: Rect {
                        x: 40,
                        y: 30,
                        width: 100,
                        height: 80,
                    },
                    clip: Rect {
                        x: 40,
                        y: 30,
                        width: 100,
                        height: 80,
                    },
                    clip_masks: Vec::new(),
                },
            ],
            finishes_move: false,
            desktop_buffers: vec![1],
            app_presentations: vec![AppPresentation {
                surface_id: 7,
                revision: 3,
                previous: None,
            }],
            routing: vec![RoutingNode {
                surface_id: 7,
                window_group: 7,
                bounds: Rect::default(),
                input: Vec::new(),
            }],
            focused_surface: 7,
        };
        let grab = MoveGrab {
            surface_id: 7,
            move_token: 1,
            underlay_buffer_id: 2,
            down: (20, 20),
            origin: (40, 30),
            offset: (10, 10),
            limits: (0, 0, 200, 100),
            ending: false,
        };

        assert_eq!(
            scene.project_move(grab),
            SceneMove::Active((7, (10, 10), 2))
        );

        scene.revoke_surface(7);

        assert_eq!(scene.project_move(grab), SceneMove::MissingGroup(7));
        assert_eq!(scene.nodes.len(), 1);
        assert!(scene.routing.is_empty());
        assert!(scene.app_presentations.is_empty());
        assert_eq!(scene.focused_surface, 0);
        assert_eq!(
            scene.damage,
            Rect {
                x: 40,
                y: 30,
                width: 100,
                height: 80,
            }
        );
    }

    fn content(serial: u64, buffer_id: u32, size: (u32, u32)) -> Content {
        Content {
            revision: buffer_id as u64,
            configure_serial: serial,
            buffer_id,
            size: Size {
                width: size.0,
                height: size.1,
            },
        }
    }

    fn choice(result: Option<ForeignChoice>) -> (u32, (u32, u32), bool) {
        let choice = result.expect("content available");
        (
            choice.buffer_id,
            (choice.size.width, choice.size.height),
            choice.adopt,
        )
    }

    #[test]
    fn exact_serial_pending_is_composited_at_its_size_and_adopted() {
        let pending = content(5, 11, (800, 600));
        let current = content(4, 10, (400, 300));
        assert_eq!(
            choice(select_foreign_content(Some(&pending), Some(&current), 5)),
            (11, (800, 600), true)
        );
    }

    #[test]
    fn exact_serial_already_current_is_shown_without_adopting() {
        // pending is at a different serial; the node's serial matches current.
        let pending = content(6, 11, (800, 600));
        let current = content(5, 10, (640, 480));
        assert_eq!(
            choice(select_foreign_content(Some(&pending), Some(&current), 5)),
            (10, (640, 480), false)
        );
    }

    #[test]
    fn in_flight_serial_holds_last_current_verbatim_no_stretch() {
        // The node requests serial 7, which neither buffer has drawn yet. The
        // previous presented frame (serial 5, 400x300) is held at its OWN size,
        // never stretched to the requested geometry, and never adopted.
        let pending = content(6, 11, (800, 600));
        let current = content(5, 10, (400, 300));
        assert_eq!(
            choice(select_foreign_content(Some(&pending), Some(&current), 7)),
            (10, (400, 300), false)
        );
    }

    #[test]
    fn first_frame_with_no_current_is_skipped() {
        // Only a pending buffer at a non-matching serial exists (no presented
        // frame yet): nothing to hold, so the node is skipped rather than
        // showing a staged buffer the node's serial never requested.
        let pending = content(6, 11, (800, 600));
        assert!(select_foreign_content(Some(&pending), None, 7).is_none());
        assert!(select_foreign_content(None, None, 7).is_none());
    }

    fn move_grab(token: u64, ending: bool) -> MoveGrab {
        MoveGrab {
            surface_id: 7,
            move_token: token,
            underlay_buffer_id: 2,
            down: (0, 0),
            origin: (0, 0),
            offset: (0, 0),
            limits: (0, 0, 100, 100),
            ending,
        }
    }

    #[test]
    fn move_retires_only_on_matching_token_of_an_ending_grab() {
        // Ending grab, exact token → finalizes.
        assert!(move_grab_finishes(Some(move_grab(5, true)), 5));
        // Ending grab, different token → not this commit.
        assert!(!move_grab_finishes(Some(move_grab(5, true)), 6));
        // Not yet ending (MoveComplete not sent) → never finalizes.
        assert!(!move_grab_finishes(Some(move_grab(5, false)), 5));
        // Zero commit token (no move finalizing) never matches a live grab.
        assert!(!move_grab_finishes(Some(move_grab(5, true)), 0));
        // No active grab.
        assert!(!move_grab_finishes(None, 5));
    }
}
