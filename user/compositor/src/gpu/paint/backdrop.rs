//! Backdrop capture, Gaussian blur and the retained clean-prefix cache for paint lowering.
//!
//! `paint` owns the display-list walk and decides where a backdrop effect applies; this module
//! owns how that backdrop is snapshotted, blurred, cached and drawn back as paint layers.

use std::{io, rc::Rc};

use display_proto::{ClipMask, DisplayCommand, Rect, TextureFormat, TextureRect};
use linux_uapi::drm::VirglResource;

use super::{
    PaintLayer, PaintTexture, clipped_rect, expand_rect, invalid, shape_masks, texture_rect,
    union_rect,
};
use crate::gpu::{GpuRenderer, TextureLayer, TextureMode, TextureSampling, TextureWrap};

pub(super) struct PreparedBlur<'a> {
    pub(super) texture: crate::gpu::EffectScratch<'a>,
    source_region: Rect,
    scale: [f32; 2],
    pub(super) vertical_texel_step: f32,
}

impl PreparedBlur<'_> {
    pub(super) fn source_rect(&self, region: Rect) -> TextureRect {
        TextureRect {
            x: (region.x as f64 - self.source_region.x as f64) as f32 / self.scale[0],
            y: (region.y as f64 - self.source_region.y as f64) as f32 / self.scale[1],
            width: region.width as f32 / self.scale[0],
            height: region.height as f32 / self.scale[1],
        }
    }
}

impl GpuRenderer {
    pub(super) fn snapshot_backdrop_region(
        &self,
        target: &VirglResource,
        inherited: Option<&VirglResource>,
        region: Rect,
    ) -> io::Result<crate::gpu::EffectScratch<'_>> {
        let snapshot = self.take_effect_scratch(target.width(), target.height())?;
        let copy = |texture| TextureLayer {
            texture,
            source: texture_rect(region),
            bounds: region,
            clip: region,
            clip_masks: &[],
            clip_offset: (0, 0),
            color: [1.0; 4],
            mode: TextureMode::Color,
            sampling: TextureSampling::Nearest,
            wrap: TextureWrap::Edge,
        };
        // 1. Clear pooled pixels outside the bounded read region so the linear
        //    downsample pass cannot pick up an older effect at its edge.
        // 2. Copy only the blur's visible output plus sampling support with
        //    REPLACE; source-over would accumulate an older translucent backdrop.
        // 3. Nested opacity then composites its local target over the inherited
        //    backdrop in the same bounded region.
        let source = inherited.unwrap_or(target);
        self.render_layers_with_blend(
            &snapshot,
            &[copy(source)],
            true,
            Some(crate::gpu::REPLACE_BLEND),
        )?;
        if inherited.is_some() {
            self.render_layers(&snapshot, &[copy(target)], false)?;
        }
        Ok(snapshot)
    }

    pub(super) fn prepare_gaussian_blur<'a>(
        &'a self,
        source: &VirglResource,
        source_region: Rect,
        radius: f32,
    ) -> io::Result<PreparedBlur<'a>> {
        let reduction = (radius.max(0.0) / 4.0).max(1.0);
        let width = ((source_region.width as f32 / reduction).ceil() as u32).max(1);
        let height = ((source_region.height as f32 / reduction).ceil() as u32).max(1);
        let scale = [
            source_region.width as f32 / width as f32,
            source_region.height as f32 / height as f32,
        ];
        let reduced = self.take_effect_scratch(width, height)?;
        let local = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        // 1. Reduce large-radius input until sigma=radius/2 is at most two
        // texture pixels; sparse full-resolution taps otherwise become visible
        // copies of text and icons instead of a continuous blur.
        // 2. Apply the normalized 13-coefficient Gaussian horizontally using
        // seven bilinear fetches.
        // 3. Keep the horizontal result reduced; the vertical pass is applied
        // by the caller while mapping its exact output rectangle back to screen.
        self.render(
            &reduced,
            &[TextureLayer {
                texture: source,
                source: texture_rect(source_region),
                bounds: local,
                clip: local,
                clip_masks: &[],
                clip_offset: (0, 0),
                color: [1.0; 4],
                mode: TextureMode::Color,
                sampling: TextureSampling::Linear,
                wrap: TextureWrap::Edge,
            }],
        )?;
        let horizontal = self.take_effect_scratch(width, height)?;
        let horizontal_texel_step = radius.max(0.0) / (4.0 * scale[0]);
        self.render(
            &horizontal,
            &[TextureLayer {
                texture: &reduced,
                source: texture_rect(local),
                bounds: local,
                clip: local,
                clip_masks: &[],
                clip_offset: (0, 0),
                color: [1.0; 4],
                mode: TextureMode::Gaussian {
                    texel_step: [horizontal_texel_step, 0.0],
                },
                sampling: TextureSampling::Linear,
                wrap: TextureWrap::Edge,
            }],
        )?;
        Ok(PreparedBlur {
            texture: horizontal,
            source_region,
            scale,
            vertical_texel_step: radius.max(0.0) / (4.0 * scale[1]),
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "prefix reconstruction preserves the exact recursive display-list context"
    )]
    pub(super) fn rebuild_clean_prefix<'a>(
        &'a self,
        target: &VirglResource,
        commands: &[DisplayCommand<'_>],
        prefixes: &[(usize, &[u8])],
        texture: &mut dyn FnMut(u32) -> Option<(&'a VirglResource, TextureFormat)>,
        excluded_group: Option<u32>,
        cache_owner: (u64, u32),
        mut clip_stack: Vec<ClipMask>,
        retained_clip_depth: usize,
        group: Option<u32>,
        backdrop: Option<&VirglResource>,
        snapshot_rect: Rect,
    ) -> io::Result<crate::gpu::EffectScratch<'a>> {
        // 1. A retained target contains the previous frame's final effects, so
        // neither a blur nor an opacity group's inherited backdrop may sample it.
        // 2. Rebuild only the command prefix preceding the effect into transparent
        // scratch, replacing the retained damage clip with the exact read region.
        // 3. Composite the already-clean inherited backdrop once, yielding the
        // same prefix pixels a complete repaint would expose to backdrop-filter.
        clip_stack = backdrop_prefix_clips(clip_stack, retained_clip_depth, snapshot_rect)?;
        let local = self.take_effect_scratch(target.width(), target.height())?;
        // Prefix clips bound every draw, but a nested linear filter may read a
        // half texel beyond that bound. Clear the whole pooled target once so
        // such reads resolve to transparent pixels, never an older final frame.
        self.render(&local, &[])?;
        self.render_commands(
            &local,
            commands,
            prefixes,
            texture,
            excluded_group,
            Some(cache_owner),
            clip_stack,
            0,
            group,
            backdrop,
            true,
        )?;
        self.snapshot_backdrop_region(&local, backdrop, snapshot_rect)
    }

    pub(super) fn cached_backdrop(
        &self,
        owner: (u64, u32),
        command_slot: usize,
        prefix: &[u8],
        rect: Rect,
    ) -> Option<Rc<VirglResource>> {
        self.backdrop_cache
            .borrow()
            .iter()
            .find(|entry| {
                entry.owner == owner
                    && entry.command_slot == command_slot
                    && entry.prefix == prefix
                    && entry.texture.width() == rect.width
                    && entry.texture.height() == rect.height
            })
            .map(|entry| Rc::clone(&entry.texture))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn populate_backdrop_cache(
        &self,
        source: &VirglResource,
        clips: &[ClipMask],
        retained_clip_depth: usize,
        rect: Rect,
        radius: f32,
        owner: (u64, u32),
        command_slot: usize,
        prefix: &[u8],
    ) -> io::Result<Rc<VirglResource>> {
        let stable_clips = clips
            .get(retained_clip_depth..)
            .ok_or_else(|| invalid("retained backdrop clip depth invalid"))?;
        let visible = clipped_rect(stable_clips, rect, source.width(), source.height())
            .ok_or_else(|| invalid("visible backdrop cache unexpectedly empty"))?;
        let source_region =
            backdrop_snapshot_rect(stable_clips, rect, radius, source.width(), source.height())
                .ok_or_else(|| invalid("backdrop cache source unexpectedly empty"))?;
        let blur = self.prepare_gaussian_blur(source, source_region, radius)?;
        let local = Rect {
            x: visible.x - rect.x,
            y: visible.y - rect.y,
            width: visible.width,
            height: visible.height,
        };
        let mut cache = self.backdrop_cache.borrow_mut();
        let index = match cache
            .iter()
            .position(|entry| entry.owner == owner && entry.command_slot == command_slot)
        {
            Some(index) => index,
            None => {
                if cache.len() == crate::gpu::BACKDROP_CACHE_CAPACITY {
                    cache.remove(0);
                }
                let mut stored_prefix = Vec::new();
                stored_prefix
                    .try_reserve_exact(prefix.len())
                    .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
                cache.push(crate::gpu::BackdropCacheEntry {
                    owner,
                    command_slot,
                    prefix: stored_prefix,
                    texture: Rc::new(self.context.create_texture(rect.width, rect.height)?),
                });
                cache.len() - 1
            }
        };
        let entry = &mut cache[index];
        if entry.texture.width() != rect.width || entry.texture.height() != rect.height {
            entry.texture = Rc::new(self.context.create_texture(rect.width, rect.height)?);
        }
        self.render(
            &entry.texture,
            &[TextureLayer {
                texture: &blur.texture,
                source: blur.source_rect(visible),
                bounds: local,
                clip: local,
                clip_masks: &[],
                clip_offset: (0, 0),
                color: [1.0; 4],
                mode: TextureMode::Gaussian {
                    texel_step: [0.0, blur.vertical_texel_step],
                },
                sampling: TextureSampling::Linear,
                wrap: TextureWrap::Edge,
            }],
        )?;
        entry.prefix.clear();
        entry
            .prefix
            .try_reserve(prefix.len())
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        entry.prefix.extend_from_slice(prefix);
        Ok(Rc::clone(&entry.texture))
    }

    pub(super) fn draw_backdrop(
        &self,
        target: &VirglResource,
        texture: &VirglResource,
        clips: &[ClipMask],
        rect: Rect,
        radii: [display_proto::CornerRadius; 4],
        radius: f32,
    ) -> io::Result<()> {
        let screen = Rect {
            x: 0,
            y: 0,
            width: target.width(),
            height: target.height(),
        };
        let masks = shape_masks(clips, rect, radii)?;
        let source_region =
            backdrop_snapshot_rect(clips, rect, radius, texture.width(), texture.height())
                .ok_or_else(|| invalid("visible backdrop source unexpectedly empty"))?;
        let blur = self.prepare_gaussian_blur(texture, source_region, radius)?;
        self.render_layers(
            target,
            &[TextureLayer {
                texture: &blur.texture,
                source: blur.source_rect(rect),
                bounds: rect,
                clip: screen,
                clip_masks: &masks,
                clip_offset: (0, 0),
                color: [1.0; 4],
                mode: TextureMode::Gaussian {
                    texel_step: [0.0, blur.vertical_texel_step],
                },
                sampling: TextureSampling::Linear,
                wrap: TextureWrap::Edge,
            }],
            false,
        )
    }
}

pub(super) fn backdrop_snapshot_rect(
    clips: &[ClipMask],
    rect: Rect,
    radius: f32,
    width: u32,
    height: u32,
) -> Option<Rect> {
    let visible = clipped_rect(clips, rect, width, height)?;
    let screen = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    crate::gpu::intersect(
        expand_rect(visible, display_proto::blur_support(radius), [0.0; 2]),
        screen,
    )
}

pub(super) fn backdrop_read_rect(
    commands: &[DisplayCommand<'_>],
    inherited: &[ClipMask],
    width: u32,
    height: u32,
) -> io::Result<Option<Rect>> {
    let mut clips = inherited.to_vec();
    let mut read = None;
    for command in commands {
        match *command {
            DisplayCommand::PushClip(mask) => clips.push(mask),
            DisplayCommand::PopClip => {
                clips
                    .pop()
                    .ok_or_else(|| invalid("display-list clip stack underflow"))?;
            }
            DisplayCommand::BackdropBlur { rect, radius, .. } => {
                if let Some(region) = backdrop_snapshot_rect(&clips, rect, radius, width, height) {
                    read = Some(read.map_or(region, |read| union_rect(read, region)));
                }
            }
            _ => {}
        }
    }
    Ok(read)
}

fn backdrop_prefix_clips(
    mut clips: Vec<ClipMask>,
    retained_clip_depth: usize,
    snapshot_rect: Rect,
) -> io::Result<Vec<ClipMask>> {
    if retained_clip_depth > clips.len() {
        return Err(invalid("retained backdrop prefix clip depth invalid"));
    }
    clips.drain(..retained_clip_depth);
    clips.push(ClipMask {
        rect: snapshot_rect,
        radii: [display_proto::CornerRadius::default(); 4],
    });
    Ok(clips)
}

pub(super) fn cached_backdrop_layer<'a>(
    texture: Rc<VirglResource>,
    clips: &[ClipMask],
    rect: Rect,
    radii: [display_proto::CornerRadius; 4],
) -> io::Result<PaintLayer<'a>> {
    Ok(PaintLayer {
        texture: PaintTexture::Cached(texture),
        source: TextureRect {
            x: 0.0,
            y: 0.0,
            width: rect.width as f32,
            height: rect.height as f32,
        },
        bounds: rect,
        masks: shape_masks(clips, rect, radii)?,
        color: [1.0; 4],
        mode: TextureMode::Color,
        sampling: TextureSampling::Linear,
        wrap: TextureWrap::Edge,
    })
}

#[cfg(test)]
mod tests {
    use super::{backdrop_prefix_clips, backdrop_read_rect, backdrop_snapshot_rect};
    use display_proto::{ClipMask, CornerRadius, DisplayCommand, Rect};

    #[test]
    fn backdrop_snapshot_is_bounded_by_damage_plus_blur_support() {
        let damage = ClipMask {
            rect: Rect {
                x: 100,
                y: 120,
                width: 20,
                height: 30,
            },
            radii: [CornerRadius::default(); 4],
        };
        assert_eq!(
            backdrop_snapshot_rect(
                &[damage],
                Rect {
                    x: 40,
                    y: 50,
                    width: 300,
                    height: 300,
                },
                24.0,
                3008,
                1692,
            ),
            Some(Rect {
                x: 64,
                y: 84,
                width: 92,
                height: 102,
            })
        );
    }

    #[test]
    fn retained_backdrop_rebuild_replaces_damage_with_sampling_support() {
        let damage = ClipMask {
            rect: Rect {
                x: 100,
                y: 120,
                width: 20,
                height: 30,
            },
            radii: [CornerRadius::default(); 4],
        };
        let window = ClipMask {
            rect: Rect {
                x: 40,
                y: 50,
                width: 300,
                height: 300,
            },
            radii: [CornerRadius { x: 18, y: 18 }; 4],
        };
        let support = Rect {
            x: 64,
            y: 84,
            width: 92,
            height: 102,
        };
        let clips = backdrop_prefix_clips(vec![damage, window], 1, support).unwrap();
        assert_eq!(clips[0], window);
        assert_eq!(clips[1].rect, support);
        assert_eq!(clips[1].radii, [CornerRadius::default(); 4]);
    }

    #[test]
    fn opacity_backdrop_read_is_bounded_to_descendant_blur_support() {
        let damage = ClipMask {
            rect: Rect {
                x: 100,
                y: 120,
                width: 20,
                height: 30,
            },
            radii: [CornerRadius::default(); 4],
        };
        let commands = [
            DisplayCommand::PushOpacity(0.5),
            DisplayCommand::BackdropBlur {
                rect: Rect {
                    x: 40,
                    y: 50,
                    width: 300,
                    height: 300,
                },
                radii: [CornerRadius::default(); 4],
                radius: 24.0,
            },
            DisplayCommand::PopOpacity,
        ];
        assert_eq!(
            backdrop_read_rect(&commands, &[damage], 3008, 1692).unwrap(),
            Some(Rect {
                x: 64,
                y: 84,
                width: 92,
                height: 102,
            })
        );
        assert_eq!(
            backdrop_read_rect(
                &[DisplayCommand::SolidRect {
                    rect: damage.rect,
                    radii: [CornerRadius::default(); 4],
                    color: 0xff00_0000,
                }],
                &[damage],
                3008,
                1692,
            )
            .unwrap(),
            None
        );
    }
}
