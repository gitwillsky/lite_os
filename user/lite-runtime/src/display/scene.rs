//! Desktop flat-scene construction and atomic commit.

use std::io;

use display_proto::{
    ClipMask, ClipMasks, MAX_NODE_CLIP_MASKS, Rect, Rectangles, SceneCommit, SceneNode,
    SceneNodeKind, send_message,
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
    /// - `move_token`: Grab token to echo when this commit applies a completed
    ///   compositor move; zero when no move is finalizing.
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
        focused_surface: u32,
        move_token: u64,
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
        let no_clip_masks: [ClipMask; 0] = [];
        let mut nodes = Vec::with_capacity(1 + windows.len() * 3 + foreign.len() + overlays.len());
        nodes.push(SceneNode {
            kind: SceneNodeKind::DisplayList,
            window_group: 0,
            source_id: 0,
            configure_serial: 0,
            bounds: full,
            clip: full,
            clip_masks: ClipMasks::from_slice(&no_clip_masks),
            opaque: Some(full),
            input: Rectangles::from_slice(&full_input),
            damage: Rectangles::from_slice(damage),
        });
        let window_frames: Vec<[Rect; 1]> = windows.iter().map(|window| [window.frame]).collect();
        let foreign_bounds: Vec<[Rect; 1]> = foreign.iter().map(|layer| [layer.bounds]).collect();
        let foreign_clip_masks = foreign
            .iter()
            .map(|layer| {
                let window = windows
                    .iter()
                    .find(|window| window.surface_id == layer.surface_id);
                scene_clip_masks(layer, window)
            })
            .collect::<io::Result<Vec<_>>>()?;
        for (window, frame_input) in windows.iter().zip(&window_frames) {
            nodes.push(SceneNode {
                kind: SceneNodeKind::DisplayList,
                window_group: window.surface_id,
                source_id: 0,
                configure_serial: 0,
                bounds: full,
                clip: window.frame,
                clip_masks: ClipMasks::from_slice(std::slice::from_ref(&window.clip_mask)),
                opaque: None,
                input: Rectangles::from_slice(frame_input),
                damage: Rectangles::from_slice(&no_damage),
            });
            if let Some((index, layer)) = foreign
                .iter()
                .enumerate()
                .find(|(_, layer)| layer.surface_id == window.surface_id)
            {
                nodes.push(SceneNode {
                    kind: SceneNodeKind::ForeignSurface,
                    window_group: layer.surface_id,
                    source_id: layer.surface_id,
                    configure_serial: layer.configure_serial,
                    bounds: layer.bounds,
                    clip: layer.clip,
                    clip_masks: ClipMasks::from_slice(&foreign_clip_masks[index]),
                    opaque: Some(layer.bounds),
                    input: Rectangles::from_slice(&foreign_bounds[index]),
                    damage: Rectangles::from_slice(&no_damage),
                });
                if !layer.desktop_input.is_empty() {
                    nodes.push(SceneNode {
                        kind: SceneNodeKind::DisplayList,
                        window_group: layer.surface_id,
                        source_id: 0,
                        configure_serial: 0,
                        bounds: full,
                        clip: Rect::default(),
                        clip_masks: ClipMasks::from_slice(&no_clip_masks),
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
                kind: SceneNodeKind::DisplayList,
                window_group: 0,
                source_id: 0,
                configure_serial: 0,
                bounds: full,
                clip: overlay.rect,
                clip_masks: ClipMasks::from_slice(std::slice::from_ref(&overlay.clip_mask)),
                opaque: None,
                input: Rectangles::from_slice(input),
                damage: Rectangles::from_slice(&no_damage),
            });
        }
        let message = SceneCommit::encode(
            &mut self.staging,
            revision,
            self.output_serial,
            focused_surface,
            move_token,
            &nodes,
        )
        .ok_or_else(|| io::Error::other("scene encoding failed"))?;
        send_message(&self.stream, message)?;
        self.submitted.push_back(revision);
        Ok(())
    }
}

/// Adds the owning window's outer shape to an embedded surface's inherited CSS clips.
///
/// The desktop display list already uses `WindowFrame::clip_mask`, but foreign pixels are a
/// separate scene node and therefore do not inherit that node's clip. Omitting this mask lets an
/// opaque client texture overwrite the frame's rounded bottom corners.
fn scene_clip_masks(
    layer: &ForeignLayer,
    window: Option<&WindowFrame>,
) -> io::Result<Vec<ClipMask>> {
    let mut masks = layer.clip_masks.clone();
    let Some(mask) = window.map(|window| window.clip_mask) else {
        return Ok(masks);
    };
    if masks.contains(&mask) {
        return Ok(masks);
    }
    if masks.len() == MAX_NODE_CLIP_MASKS {
        return Err(io::Error::other(
            "foreign surface clip chain leaves no room for its window shape",
        ));
    }
    masks.push(mask);
    Ok(masks)
}

#[cfg(test)]
mod tests {
    use display_proto::{CornerRadius, Rect};

    use super::{ForeignLayer, WindowFrame, scene_clip_masks};

    #[test]
    fn foreign_surface_includes_owning_window_shape() {
        let bounds = Rect {
            x: 10,
            y: 60,
            width: 400,
            height: 300,
        };
        let window_mask = display_proto::ClipMask {
            rect: Rect {
                x: 8,
                y: 8,
                width: 404,
                height: 352,
            },
            radii: [CornerRadius { x: 14, y: 14 }; 4],
        };
        let layer = ForeignLayer {
            surface_id: 7,
            configure_serial: 1,
            bounds,
            clip: bounds,
            clip_masks: Vec::new(),
            desktop_input: Vec::new(),
            desktop_hit_start: 0,
        };
        let window = WindowFrame {
            surface_id: 7,
            frame: window_mask.rect,
            clip_mask: window_mask,
        };

        assert_eq!(
            scene_clip_masks(&layer, Some(&window)).unwrap(),
            [window_mask]
        );
    }
}
