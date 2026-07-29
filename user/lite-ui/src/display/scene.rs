//! Desktop flat-scene construction and atomic commit.

use std::io;

use display_proto::{
    MAX_MESSAGE, Rect, Rectangles, SceneCommit, SceneNode, SceneNodeKind, send_message,
};

use super::{Display, ForeignLayer, Overlay, WindowFrame};

impl Display {
    /// Commits desktop pixels interleaved with ready app surface layers.
    ///
    /// Node order is the z-stack: full desktop pixels; for each window its
    /// frame, foreign client and later-painted desktop input; then global
    /// chrome. An input-only node has an empty paint clip, preserving standard
    /// DOM hit order without obscuring embedded client pixels.
    ///
    /// # Parameters
    ///
    /// - `buffer_id`: Desktop buffer containing the complete retained raster.
    /// - `focused_surface`: Surface receiving keyboard events after presentation.
    /// - `foreign`: Ready embedded client surfaces in React paint order.
    /// - `windows`: System window frames in React z-order.
    /// - `overlays`: Fixed global chrome repainted above every window.
    /// - `damage`: Physical desktop rectangles changed by this revision.
    ///
    /// # Returns
    ///
    /// Returns after the complete scene snapshot is sent asynchronously.
    ///
    /// # Errors
    ///
    /// Returns an error when revision allocation, scene encoding or socket
    /// delivery fails.
    pub fn commit_desktop(
        &mut self,
        buffer_id: u32,
        focused_surface: u32,
        foreign: &[ForeignLayer],
        windows: &[WindowFrame],
        overlays: &[Overlay],
        damage: &[Rect],
    ) -> io::Result<()> {
        let revision = self.next_revision()?;
        let full = Rect {
            x: 0,
            y: 0,
            width: self.physical.width,
            height: self.physical.height,
        };
        let full_input = [full];
        let no_damage = [];
        let mut nodes =
            Vec::with_capacity(1 + windows.len() * 3 + foreign.len() + overlays.len());
        nodes.push(SceneNode {
            kind: SceneNodeKind::Pixels,
            window_group: 0,
            source_id: buffer_id,
            corner_radius: 0,
            configure_serial: 0,
            bounds: full,
            clip: full,
            opaque: Some(full),
            input: Rectangles::from_slice(&full_input),
            damage: Rectangles::from_slice(damage),
        });
        let window_frames: Vec<[Rect; 1]> = windows.iter().map(|window| [window.frame]).collect();
        let foreign_bounds: Vec<[Rect; 1]> = foreign.iter().map(|layer| [layer.bounds]).collect();
        for (window, frame_input) in windows.iter().zip(&window_frames) {
            nodes.push(SceneNode {
                kind: SceneNodeKind::Pixels,
                window_group: window.surface_id,
                source_id: buffer_id,
                corner_radius: window.corner_radius,
                configure_serial: 0,
                bounds: full,
                clip: window.frame,
                opaque: None,
                input: Rectangles::from_slice(frame_input),
                damage: Rectangles::from_slice(&no_damage),
            });
            if let Some((index, layer)) = foreign
                .iter()
                .enumerate()
                .find(|(_, layer)| layer.surface_id == window.surface_id)
                && self
                    .ready
                    .contains(&(layer.surface_id, layer.configure_serial))
            {
                nodes.push(SceneNode {
                    kind: SceneNodeKind::ForeignSurface,
                    window_group: layer.surface_id,
                    source_id: layer.surface_id,
                    corner_radius: 0,
                    configure_serial: layer.configure_serial,
                    bounds: layer.bounds,
                    clip: full,
                    opaque: Some(layer.bounds),
                    input: Rectangles::from_slice(&foreign_bounds[index]),
                    damage: Rectangles::from_slice(&no_damage),
                });
                if !layer.desktop_input.is_empty() {
                    nodes.push(SceneNode {
                        kind: SceneNodeKind::Pixels,
                        window_group: layer.surface_id,
                        source_id: buffer_id,
                        corner_radius: 0,
                        configure_serial: 0,
                        bounds: full,
                        clip: Rect::default(),
                        opaque: None,
                        input: Rectangles::from_slice(&layer.desktop_input),
                        damage: Rectangles::from_slice(&no_damage),
                    });
                }
            }
        }
        let overlay_inputs: Vec<[Rect; 1]> =
            overlays.iter().map(|overlay| [overlay.rect]).collect();
        for (overlay, input) in overlays.iter().zip(&overlay_inputs) {
            nodes.push(SceneNode {
                kind: SceneNodeKind::Pixels,
                window_group: 0,
                source_id: buffer_id,
                corner_radius: overlay.corner_radius,
                configure_serial: 0,
                bounds: full,
                clip: overlay.rect,
                opaque: None,
                input: Rectangles::from_slice(input),
                damage: Rectangles::from_slice(&no_damage),
            });
        }
        let mut output = [0u8; MAX_MESSAGE];
        let message = SceneCommit::encode(&mut output, revision, focused_surface, &nodes)
            .ok_or_else(|| io::Error::other("scene encoding failed"))?;
        send_message(&self.stream, message)?;
        self.submitted.push_back(revision);
        Ok(())
    }
}
