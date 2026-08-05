//! Lowering from validated display-list primitives to GPU draws.

use std::{io, rc::Rc};

use display_proto::{
    BorderStyle, ClipMask, DisplayCommand, DisplayListCommit, ImageSampling, Rect, TextureFormat,
    TextureRect,
};
use linux_uapi::drm::VirglResource;

use super::{GpuRenderer, TextureLayer, TextureMode, TextureSampling, TextureWrap};

struct PaintLayer<'a> {
    texture: PaintTexture<'a>,
    source: TextureRect,
    bounds: Rect,
    masks: Vec<ClipMask>,
    color: [f32; 4],
    mode: TextureMode,
    sampling: TextureSampling,
    wrap: TextureWrap,
}

enum PaintTexture<'a> {
    Borrowed(&'a VirglResource),
    Cached(Rc<VirglResource>),
}

impl PaintTexture<'_> {
    fn resource(&self) -> &VirglResource {
        match self {
            Self::Borrowed(resource) => resource,
            Self::Cached(resource) => resource,
        }
    }
}

impl GpuRenderer {
    /// Executes a complete immutable CSS display list in the compositor's sole
    /// VirGL context. Geometry assembly remains on the CPU; coverage, filtering,
    /// blending, blur and final rasterization execute in GPU shaders.
    pub fn render_display_list<'a>(
        &'a self,
        target: &VirglResource,
        list: DisplayListCommit<'_>,
        base: Option<&VirglResource>,
        repair: Option<Rect>,
        cache_owner: (u64, u32),
        mut texture: impl FnMut(u32) -> Option<(&'a VirglResource, TextureFormat)>,
    ) -> io::Result<()> {
        let full_damage = list.damage.x == 0
            && list.damage.y == 0
            && list.damage.width == target.width()
            && list.damage.height == target.height();
        let retained = base.is_some() && !full_damage;
        if retained {
            let base = base.expect("retained target has an exact base");
            self.prepare_retained_target(target, base, repair, list.damage)?;
        } else if list.base_revision != 0 {
            if base.is_none() {
                return Err(invalid("retained display list has no exact base revision"));
            }
        }
        if list.base_revision != 0 && list.damage.width == 0 {
            return Ok(());
        }
        let damage_clip = retained.then(|| ClipMask {
            rect: list.damage,
            radii: [display_proto::CornerRadius::default(); 4],
        });
        self.render_display_list_filtered(
            target,
            list,
            &mut texture,
            None,
            Some(cache_owner),
            damage_clip.into_iter().collect(),
            retained,
        )
    }

    /// Renders the desktop snapshot without one movable window group.
    pub fn render_display_list_excluding<'a>(
        &'a self,
        target: &VirglResource,
        list: DisplayListCommit<'_>,
        mut texture: impl FnMut(u32) -> Option<(&'a VirglResource, TextureFormat)>,
        excluded_group: u32,
    ) -> io::Result<()> {
        self.render_display_list_filtered(
            target,
            list,
            &mut texture,
            Some(excluded_group),
            None,
            Vec::new(),
            false,
        )
    }

    fn render_display_list_filtered<'a>(
        &'a self,
        target: &VirglResource,
        list: DisplayListCommit<'_>,
        texture: &mut dyn FnMut(u32) -> Option<(&'a VirglResource, TextureFormat)>,
        excluded_group: Option<u32>,
        cache_owner: Option<(u64, u32)>,
        clip_stack: Vec<ClipMask>,
        target_initialized: bool,
    ) -> io::Result<()> {
        let mut decoded = list.commands();
        let mut commands = Vec::with_capacity(decoded.len());
        let mut prefixes = Vec::with_capacity(decoded.len());
        while let Some(command) = decoded.next() {
            commands.push(command);
            prefixes.push((commands.len() - 1, decoded.consumed_bytes()));
        }
        self.render_commands(
            target,
            &commands,
            &prefixes,
            texture,
            excluded_group,
            cache_owner,
            clip_stack,
            usize::from(target_initialized),
            None,
            None,
            target_initialized,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "recursive lowering keeps clip, group, exclusion, and backdrop ownership explicit"
    )]
    fn render_commands<'a>(
        &'a self,
        target: &VirglResource,
        commands: &[DisplayCommand<'_>],
        prefixes: &[(usize, &[u8])],
        texture: &mut dyn FnMut(u32) -> Option<(&'a VirglResource, TextureFormat)>,
        excluded_group: Option<u32>,
        cache_owner: Option<(u64, u32)>,
        mut clip_stack: Vec<ClipMask>,
        retained_clip_depth: usize,
        mut group: Option<u32>,
        backdrop: Option<&VirglResource>,
        initialized: bool,
    ) -> io::Result<()> {
        let mut layers = Vec::new();
        let mut target_initialized = initialized;
        let mut index = 0usize;
        while index < commands.len() {
            let command = commands[index];
            let (command_slot, command_prefix) = prefixes[index];
            index += 1;
            match command {
                DisplayCommand::PushGroup(id) => {
                    group = Some(id);
                    continue;
                }
                DisplayCommand::PopGroup => {
                    group = None;
                    continue;
                }
                _ => {}
            }
            if excluded_group.is_some() && group == excluded_group {
                if matches!(command, DisplayCommand::PushOpacity(_)) {
                    index = matching_opacity_pop(commands, index)? + 1;
                }
                continue;
            }
            match command {
                DisplayCommand::PushGroup(_) | DisplayCommand::PopGroup => unreachable!(),
                DisplayCommand::PushClip(mask) => clip_stack.push(mask),
                DisplayCommand::PopClip => {
                    clip_stack
                        .pop()
                        .ok_or_else(|| invalid("display-list clip stack underflow"))?;
                }
                DisplayCommand::PushOpacity(opacity) => {
                    let pop = matching_opacity_pop(commands, index)?;
                    if !commands_may_touch(
                        &commands[index..pop],
                        &clip_stack,
                        target.width(),
                        target.height(),
                    ) {
                        index = pop + 1;
                        continue;
                    }
                    self.flush(target, &mut layers, &mut target_initialized)?;
                    if !target_initialized {
                        self.render(target, &[])?;
                    }
                    let isolated_backdrop = self.snapshot_backdrop(target, backdrop)?;
                    let scratch = self.take_effect_scratch(target.width(), target.height())?;
                    self.render_commands(
                        &scratch,
                        &commands[index..pop],
                        &prefixes[index..pop],
                        texture,
                        excluded_group,
                        cache_owner,
                        clip_stack.clone(),
                        retained_clip_depth,
                        group,
                        Some(&isolated_backdrop),
                        false,
                    )?;
                    let screen = Rect {
                        x: 0,
                        y: 0,
                        width: target.width(),
                        height: target.height(),
                    };
                    self.render_layers(
                        target,
                        &[TextureLayer {
                            texture: &scratch,
                            source: texture_rect(screen),
                            bounds: screen,
                            clip: screen,
                            clip_masks: &[],
                            clip_offset: (0, 0),
                            color: [opacity; 4],
                            mode: TextureMode::Color,
                            sampling: TextureSampling::Linear,
                            wrap: TextureWrap::Edge,
                        }],
                        !target_initialized,
                    )?;
                    target_initialized = true;
                    index = pop + 1;
                }
                DisplayCommand::PopOpacity => {
                    return Err(invalid("display-list opacity group escaped its slice"));
                }
                DisplayCommand::SolidRect { rect, radii, color } => {
                    layers.push(self.solid_layer(&clip_stack, rect, radii, color, 1.0)?);
                }
                DisplayCommand::LinearGradient {
                    rect,
                    radii,
                    start,
                    end,
                    stops,
                } => {
                    let stops = stops.iter().collect::<Vec<_>>();
                    let masks = shape_masks(&clip_stack, rect, radii)?;
                    let intervals = stops
                        .windows(2)
                        .filter(|pair| pair[1].offset > pair[0].offset)
                        .collect::<Vec<_>>();
                    if intervals.is_empty() {
                        let boundary = stops[0].offset;
                        for (color, coverage) in [
                            (stops[0].color, [f32::MIN, boundary]),
                            (stops[stops.len() - 1].color, [boundary, f32::MAX]),
                        ] {
                            layers.push(gradient_layer(
                                &self.white,
                                rect,
                                &masks,
                                start,
                                end,
                                [0.0, 1.0],
                                coverage,
                                color_rgba(color, 1.0),
                                color_rgba(color, 1.0),
                            ));
                        }
                        continue;
                    }
                    for (index, pair) in intervals.iter().enumerate() {
                        layers.push(PaintLayer {
                            texture: PaintTexture::Borrowed(&self.white),
                            source: unit_source(),
                            bounds: rect,
                            masks: masks.clone(),
                            color: [1.0; 4],
                            mode: TextureMode::Gradient {
                                start,
                                end,
                                range: [pair[0].offset, pair[1].offset],
                                coverage: [
                                    if index == 0 { f32::MIN } else { pair[0].offset },
                                    if index + 1 == intervals.len() {
                                        f32::MAX
                                    } else {
                                        pair[1].offset
                                    },
                                ],
                                start_color: color_rgba(pair[0].color, 1.0),
                                end_color: color_rgba(pair[1].color, 1.0),
                            },
                            sampling: TextureSampling::Nearest,
                            wrap: TextureWrap::Edge,
                        });
                    }
                }
                DisplayCommand::Border {
                    rect,
                    radii,
                    widths,
                    colors,
                    styles,
                } => {
                    if let Some((width, color)) =
                        rounded_solid_border(radii, widths, colors, styles)
                    {
                        // Four thin edge rectangles cannot contain the arc
                        // pixels between them. The existing analytic inset
                        // shadow is the exact rounded-ring primitive and keeps
                        // focus chrome in the same GPU pipeline.
                        if let Some(layer) = self.shadow_layer(
                            target,
                            &clip_stack,
                            rect,
                            radii,
                            [0.0; 2],
                            0.0,
                            width,
                            color,
                            true,
                            1.0,
                        )? {
                            layers.push(layer);
                        }
                    } else {
                        self.border_layers(
                            &mut layers,
                            &clip_stack,
                            rect,
                            radii,
                            widths,
                            colors,
                            styles,
                            1.0,
                        )?;
                    }
                }
                DisplayCommand::BoxShadow {
                    rect,
                    radii,
                    offset,
                    blur,
                    spread,
                    color,
                    inset,
                } => {
                    if let Some(layer) = self.shadow_layer(
                        target,
                        &clip_stack,
                        rect,
                        radii,
                        offset,
                        blur,
                        spread,
                        color,
                        inset,
                        1.0,
                    )? {
                        layers.push(layer);
                    }
                }
                DisplayCommand::Image {
                    texture_id,
                    source,
                    destination,
                    radii,
                    opacity,
                    sampling,
                    repeat,
                } => {
                    let (resource, format) = texture(texture_id)
                        .ok_or_else(|| invalid("display-list image texture disappeared"))?;
                    if format != TextureFormat::Bgra8Premultiplied {
                        return Err(invalid("display-list image texture format"));
                    }
                    layers.push(PaintLayer {
                        texture: PaintTexture::Borrowed(resource),
                        source,
                        bounds: destination,
                        masks: shape_masks(&clip_stack, destination, radii)?,
                        color: [opacity; 4],
                        mode: TextureMode::Color,
                        sampling: match sampling {
                            ImageSampling::Nearest => TextureSampling::Nearest,
                            ImageSampling::Linear => TextureSampling::Linear,
                        },
                        wrap: TextureWrap::Background(repeat),
                    });
                }
                DisplayCommand::GlyphRun {
                    texture_id,
                    color,
                    offset,
                    blur,
                    glyphs,
                } => {
                    let (resource, format) = texture(texture_id)
                        .ok_or_else(|| invalid("display-list glyph texture disappeared"))?;
                    if format != TextureFormat::R8 {
                        return Err(invalid("display-list glyph texture format"));
                    }
                    let glyphs = glyphs.iter().collect::<Vec<_>>();
                    if blur > 0.0 {
                        if !glyphs.iter().any(|glyph| {
                            clip_intersects(
                                &clip_stack,
                                expand_rect(offset_rect(glyph.destination, offset), blur, [0.0; 2]),
                                target.width(),
                                target.height(),
                            )
                        }) {
                            continue;
                        }
                        self.flush(target, &mut layers, &mut target_initialized)?;
                        let scratch = self.take_effect_scratch(target.width(), target.height())?;
                        let screen = Rect {
                            x: 0,
                            y: 0,
                            width: target.width(),
                            height: target.height(),
                        };
                        let masks = clip_stack.clone();
                        let glyph_layers = glyphs
                            .iter()
                            .map(|glyph| TextureLayer {
                                texture: resource,
                                source: texture_rect(glyph.source),
                                bounds: offset_rect(glyph.destination, offset),
                                clip: screen,
                                clip_masks: &masks,
                                clip_offset: (0, 0),
                                color: [1.0; 4],
                                mode: TextureMode::Mask,
                                sampling: TextureSampling::Linear,
                                wrap: TextureWrap::Edge,
                            })
                            .collect::<Vec<_>>();
                        self.render(&scratch, &glyph_layers)?;
                        self.render_layers(
                            target,
                            &[TextureLayer {
                                texture: &scratch,
                                source: texture_rect(screen),
                                bounds: screen,
                                clip: screen,
                                clip_masks: &[],
                                clip_offset: (0, 0),
                                color: color_rgba(color, 1.0),
                                mode: TextureMode::MaskBlur { radius: blur },
                                sampling: TextureSampling::Linear,
                                wrap: TextureWrap::Edge,
                            }],
                            false,
                        )?;
                        target_initialized = true;
                        continue;
                    }
                    for glyph in glyphs {
                        layers.push(PaintLayer {
                            texture: PaintTexture::Borrowed(resource),
                            source: texture_rect(glyph.source),
                            bounds: offset_rect(glyph.destination, offset),
                            masks: clip_stack.clone(),
                            color: color_rgba(color, 1.0),
                            mode: TextureMode::Mask,
                            sampling: TextureSampling::Linear,
                            wrap: TextureWrap::Edge,
                        });
                    }
                }
                DisplayCommand::BackdropBlur {
                    rect,
                    radii,
                    radius,
                } => {
                    let Some(snapshot_rect) = backdrop_snapshot_rect(
                        &clip_stack,
                        rect,
                        radius,
                        target.width(),
                        target.height(),
                    ) else {
                        continue;
                    };
                    if let Some(owner) = cache_owner {
                        if let Some(texture) = self.cached_backdrop(
                            owner,
                            command_slot,
                            command_prefix,
                            rect,
                        ) {
                            layers.push(cached_backdrop_layer(
                                texture,
                                &clip_stack,
                                rect,
                                radii,
                            )?);
                            continue;
                        }
                    }

                    self.flush(target, &mut layers, &mut target_initialized)?;
                    if !target_initialized {
                        self.render(target, &[])?;
                    }
                    if let Some(owner) = cache_owner {
                        let texture = self.populate_backdrop_cache(
                            target,
                            backdrop,
                            &clip_stack,
                            retained_clip_depth,
                            rect,
                            radius,
                            owner,
                            command_slot,
                            command_prefix,
                        )?;
                        layers.push(cached_backdrop_layer(
                            texture,
                            &clip_stack,
                            rect,
                            radii,
                        )?);
                    } else {
                        let scratch =
                            self.snapshot_backdrop_region(target, backdrop, snapshot_rect)?;
                        self.draw_backdrop(
                            target,
                            &scratch,
                            &clip_stack,
                            rect,
                            radii,
                            radius,
                        )?;
                        target_initialized = true;
                    }
                }
            }
        }
        self.flush(target, &mut layers, &mut target_initialized)?;
        if !target_initialized {
            self.render(target, &[])?;
        }
        Ok(())
    }

    fn solid_layer<'a>(
        &'a self,
        clips: &[ClipMask],
        rect: Rect,
        radii: [display_proto::CornerRadius; 4],
        color: u32,
        opacity: f32,
    ) -> io::Result<PaintLayer<'a>> {
        Ok(PaintLayer {
            texture: PaintTexture::Borrowed(&self.white),
            source: unit_source(),
            bounds: rect,
            masks: shape_masks(clips, rect, radii)?,
            color: color_rgba(color, opacity),
            mode: TextureMode::Color,
            sampling: TextureSampling::Nearest,
            wrap: TextureWrap::Edge,
        })
    }

    fn snapshot_backdrop(
        &self,
        target: &VirglResource,
        inherited: Option<&VirglResource>,
    ) -> io::Result<super::EffectScratch<'_>> {
        self.snapshot_backdrop_region(
            target,
            inherited,
            Rect {
                x: 0,
                y: 0,
                width: target.width(),
                height: target.height(),
            },
        )
    }

    fn snapshot_backdrop_region(
        &self,
        target: &VirglResource,
        inherited: Option<&VirglResource>,
        region: Rect,
    ) -> io::Result<super::EffectScratch<'_>> {
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
        // 1. Only the blur's visible output plus sampling support can be read.
        // 2. REPLACE is required because pooled scratch pixels may be translucent;
        //    source-over would accumulate an older backdrop and create ghost blocks.
        // 3. Nested opacity then composites its local target over the inherited
        //    backdrop in the same bounded region.
        let source = inherited.unwrap_or(target);
        self.render_layers_with_blend(
            &snapshot,
            &[copy(source)],
            false,
            Some(super::REPLACE_BLEND),
        )?;
        if inherited.is_some() {
            self.render_layers(&snapshot, &[copy(target)], false)?;
        }
        Ok(snapshot)
    }

    fn cached_backdrop(
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
    fn populate_backdrop_cache(
        &self,
        target: &VirglResource,
        inherited: Option<&VirglResource>,
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
        let visible = clipped_rect(stable_clips, rect, target.width(), target.height())
            .ok_or_else(|| invalid("visible backdrop cache unexpectedly empty"))?;
        let snapshot_rect =
            backdrop_snapshot_rect(stable_clips, rect, radius, target.width(), target.height())
                .ok_or_else(|| invalid("backdrop cache source unexpectedly empty"))?;
        let scratch = self.snapshot_backdrop_region(target, inherited, snapshot_rect)?;
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
                if cache.len() == super::BACKDROP_CACHE_CAPACITY {
                    cache.remove(0);
                }
                let mut stored_prefix = Vec::new();
                stored_prefix
                    .try_reserve_exact(prefix.len())
                    .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
                cache.push(super::BackdropCacheEntry {
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
                texture: &scratch,
                source: texture_rect(visible),
                bounds: local,
                clip: local,
                clip_masks: &[],
                clip_offset: (0, 0),
                color: [1.0; 4],
                mode: TextureMode::Blur { radius },
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

    fn draw_backdrop(
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
        self.render_layers(
            target,
            &[TextureLayer {
                texture,
                source: texture_rect(rect),
                bounds: rect,
                clip: screen,
                clip_masks: &masks,
                clip_offset: (0, 0),
                color: [1.0; 4],
                mode: TextureMode::Blur { radius },
                sampling: TextureSampling::Linear,
                wrap: TextureWrap::Edge,
            }],
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn border_layers<'a>(
        &'a self,
        output: &mut Vec<PaintLayer<'a>>,
        clips: &[ClipMask],
        rect: Rect,
        radii: [display_proto::CornerRadius; 4],
        widths: [f32; 4],
        colors: [u32; 4],
        styles: [BorderStyle; 4],
        opacity: f32,
    ) -> io::Result<()> {
        let outer_masks = shape_masks(clips, rect, radii)?;
        for side in 0..4 {
            let width = widths[side].ceil().max(0.0) as u32;
            if width == 0 || styles[side] == BorderStyle::None {
                continue;
            }
            for segment in border_segments(rect, side, width, styles[side]) {
                output.push(self.solid_layer(
                    &outer_masks,
                    segment,
                    [display_proto::CornerRadius::default(); 4],
                    colors[side],
                    opacity,
                )?);
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn shadow_layer<'a>(
        &'a self,
        target: &VirglResource,
        clips: &[ClipMask],
        rect: Rect,
        radii: [display_proto::CornerRadius; 4],
        offset: [f32; 2],
        blur: f32,
        spread: f32,
        color: u32,
        inset: bool,
        opacity: f32,
    ) -> io::Result<Option<PaintLayer<'a>>> {
        let distance = if inset { -spread } else { spread };
        let mask_rect = expand_rect(rect, distance, offset);
        if mask_rect.width == 0 || mask_rect.height == 0 {
            return Ok(None);
        }
        let support = display_proto::blur_support(blur);
        let effect_bounds = if inset {
            rect
        } else {
            expand_rect(mask_rect, support, [0.0; 2])
        };
        if !clip_intersects(clips, effect_bounds, target.width(), target.height()) {
            return Ok(None);
        }
        let mask_radii = radii.map(|radius| display_proto::CornerRadius {
            x: (radius.x as f32 + distance).max(0.0).round() as u32,
            y: (radius.y as f32 + distance).max(0.0).round() as u32,
        });
        let final_masks = if inset {
            shape_masks(clips, rect, radii)?
        } else {
            clips.to_vec()
        };
        Ok(Some(PaintLayer {
            texture: PaintTexture::Borrowed(&self.white),
            source: unit_source(),
            bounds: effect_bounds,
            masks: final_masks,
            color: color_rgba(color, opacity),
            mode: TextureMode::Shadow {
                mask: mask_rect,
                radii: mask_radii,
                support,
                spread: distance,
                offset,
                inset,
            },
            sampling: TextureSampling::Nearest,
            wrap: TextureWrap::Edge,
        }))
    }

    fn flush<'a>(
        &self,
        target: &VirglResource,
        layers: &mut Vec<PaintLayer<'a>>,
        initialized: &mut bool,
    ) -> io::Result<()> {
        if layers.is_empty() {
            return Ok(());
        }
        let screen = Rect {
            x: 0,
            y: 0,
            width: target.width(),
            height: target.height(),
        };
        let draws = layers
            .iter()
            .map(|layer| TextureLayer {
                texture: layer.texture.resource(),
                source: layer.source,
                bounds: layer.bounds,
                clip: screen,
                clip_masks: &layer.masks,
                clip_offset: (0, 0),
                color: layer.color,
                mode: layer.mode,
                sampling: layer.sampling,
                wrap: layer.wrap,
            })
            .collect::<Vec<_>>();
        self.render_layers(target, &draws, !*initialized)?;
        *initialized = true;
        layers.clear();
        Ok(())
    }
}

fn commands_may_touch(
    commands: &[DisplayCommand<'_>],
    inherited: &[ClipMask],
    width: u32,
    height: u32,
) -> bool {
    let mut clips = inherited.to_vec();
    for command in commands {
        let bounds = match *command {
            DisplayCommand::PushClip(mask) => {
                clips.push(mask);
                continue;
            }
            DisplayCommand::PopClip => {
                clips.pop();
                continue;
            }
            DisplayCommand::SolidRect { rect, .. }
            | DisplayCommand::LinearGradient { rect, .. }
            | DisplayCommand::Border { rect, .. }
            | DisplayCommand::Image {
                destination: rect, ..
            }
            | DisplayCommand::BackdropBlur { rect, .. } => Some(rect),
            DisplayCommand::BoxShadow {
                rect,
                offset,
                blur,
                spread,
                inset,
                ..
            } => Some(if inset {
                rect
            } else {
                expand_rect(
                    expand_rect(rect, spread, offset),
                    display_proto::blur_support(blur),
                    [0.0; 2],
                )
            }),
            DisplayCommand::GlyphRun {
                offset,
                blur,
                glyphs,
                ..
            } => glyphs
                .iter()
                .map(|glyph| expand_rect(offset_rect(glyph.destination, offset), blur, [0.0; 2]))
                .reduce(union_rect),
            DisplayCommand::PushGroup(_)
            | DisplayCommand::PopGroup
            | DisplayCommand::PushOpacity(_)
            | DisplayCommand::PopOpacity => continue,
        };
        if bounds.is_some_and(|bounds| clip_intersects(&clips, bounds, width, height)) {
            return true;
        }
    }
    false
}

fn clip_intersects(clips: &[ClipMask], bounds: Rect, width: u32, height: u32) -> bool {
    let screen = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    clips
        .iter()
        .fold(Some(screen), |visible, mask| {
            visible.and_then(|visible| super::intersect(visible, mask.rect))
        })
        .and_then(|visible| super::intersect(visible, bounds))
        .is_some()
}

fn backdrop_snapshot_rect(
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
    super::intersect(
        expand_rect(visible, display_proto::blur_support(radius), [0.0; 2]),
        screen,
    )
}

fn clipped_rect(clips: &[ClipMask], bounds: Rect, width: u32, height: u32) -> Option<Rect> {
    let screen = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    clips
        .iter()
        .fold(super::intersect(screen, bounds), |visible, mask| {
            visible.and_then(|visible| super::intersect(visible, mask.rect))
        })
}

fn union_rect(left: Rect, right: Rect) -> Rect {
    let x1 = i64::from(left.x).min(i64::from(right.x));
    let y1 = i64::from(left.y).min(i64::from(right.y));
    let x2 = (i64::from(left.x) + i64::from(left.width))
        .max(i64::from(right.x) + i64::from(right.width));
    let y2 = (i64::from(left.y) + i64::from(left.height))
        .max(i64::from(right.y) + i64::from(right.height));
    Rect {
        x: x1.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        y: y1.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        width: x2.saturating_sub(x1).min(i64::from(u32::MAX)) as u32,
        height: y2.saturating_sub(y1).min(i64::from(u32::MAX)) as u32,
    }
}

fn border_segments(rect: Rect, side: usize, width: u32, style: BorderStyle) -> Vec<Rect> {
    if style == BorderStyle::Double && width >= 3 {
        let line = (width / 3).max(1);
        return vec![
            side_rect(rect, side, line),
            inset_side_rect(rect, side, width - line, line),
        ];
    }
    let base = side_rect(rect, side, width);
    if !matches!(style, BorderStyle::Dotted | BorderStyle::Dashed) {
        return vec![base];
    }
    let horizontal = side.is_multiple_of(2);
    let length = if horizontal { base.width } else { base.height };
    let on = if style == BorderStyle::Dotted {
        width.max(1)
    } else {
        width.saturating_mul(3).max(1)
    };
    let period = on.saturating_mul(2);
    (0..length)
        .step_by(period as usize)
        .map(|offset| {
            let segment = on.min(length - offset);
            if horizontal {
                Rect {
                    x: base.x.saturating_add_unsigned(offset),
                    width: segment,
                    ..base
                }
            } else {
                Rect {
                    y: base.y.saturating_add_unsigned(offset),
                    height: segment,
                    ..base
                }
            }
        })
        .collect()
}

fn rounded_solid_border(
    radii: [display_proto::CornerRadius; 4],
    widths: [f32; 4],
    colors: [u32; 4],
    styles: [BorderStyle; 4],
) -> Option<(f32, u32)> {
    let width = widths[0];
    (radii.iter().any(|radius| radius.x != 0 && radius.y != 0)
        && width > 0.0
        && widths.iter().all(|candidate| *candidate == width)
        && colors.iter().all(|candidate| *candidate == colors[0])
        && styles.iter().all(|style| *style == BorderStyle::Solid))
    .then_some((width, colors[0]))
}

fn matching_opacity_pop(commands: &[DisplayCommand<'_>], start: usize) -> io::Result<usize> {
    let mut depth = 1usize;
    for (index, command) in commands.iter().enumerate().skip(start) {
        match command {
            DisplayCommand::PushOpacity(_) => depth += 1,
            DisplayCommand::PopOpacity => {
                depth -= 1;
                if depth == 0 {
                    return Ok(index);
                }
            }
            _ => {}
        }
    }
    Err(invalid("display-list opacity group is unbalanced"))
}

fn side_rect(rect: Rect, side: usize, width: u32) -> Rect {
    match side {
        0 => Rect {
            height: width.min(rect.height),
            ..rect
        },
        1 => Rect {
            x: rect
                .x
                .saturating_add_unsigned(rect.width.saturating_sub(width)),
            width: width.min(rect.width),
            ..rect
        },
        2 => Rect {
            y: rect
                .y
                .saturating_add_unsigned(rect.height.saturating_sub(width)),
            height: width.min(rect.height),
            ..rect
        },
        _ => Rect {
            width: width.min(rect.width),
            ..rect
        },
    }
}

fn inset_side_rect(rect: Rect, side: usize, inset: u32, width: u32) -> Rect {
    let mut edge = side_rect(rect, side, inset.saturating_add(width));
    match side {
        0 => {
            edge.y = edge.y.saturating_add_unsigned(inset);
            edge.height = width;
        }
        1 => edge.width = width,
        2 => edge.height = width,
        _ => {
            edge.x = edge.x.saturating_add_unsigned(inset);
            edge.width = width;
        }
    }
    edge
}

fn expand_rect(rect: Rect, distance: f32, offset: [f32; 2]) -> Rect {
    let x1 = rect.x as f32 + offset[0] - distance;
    let y1 = rect.y as f32 + offset[1] - distance;
    let x2 = rect.x as f32 + rect.width as f32 + offset[0] + distance;
    let y2 = rect.y as f32 + rect.height as f32 + offset[1] + distance;
    Rect {
        x: x1.round() as i32,
        y: y1.round() as i32,
        width: (x2 - x1).round().max(0.0) as u32,
        height: (y2 - y1).round().max(0.0) as u32,
    }
}

fn offset_rect(rect: Rect, offset: [f32; 2]) -> Rect {
    Rect {
        x: rect.x.saturating_add(offset[0].round() as i32),
        y: rect.y.saturating_add(offset[1].round() as i32),
        ..rect
    }
}

fn shape_masks(
    inherited: &[ClipMask],
    rect: Rect,
    radii: [display_proto::CornerRadius; 4],
) -> io::Result<Vec<ClipMask>> {
    let mut masks = inherited.to_vec();
    if radii.iter().any(|radius| radius.x != 0 && radius.y != 0) {
        if masks.len() > display_proto::MAX_NODE_CLIP_MASKS {
            return Err(invalid("rounded primitive exceeds GPU clip depth"));
        }
        masks.push(ClipMask { rect, radii });
    }
    Ok(masks)
}

fn texture_rect(rect: Rect) -> TextureRect {
    TextureRect {
        x: rect.x as f32,
        y: rect.y as f32,
        width: rect.width as f32,
        height: rect.height as f32,
    }
}

#[allow(clippy::too_many_arguments)]
fn gradient_layer<'a>(
    texture: &'a VirglResource,
    rect: Rect,
    masks: &[ClipMask],
    start: [f32; 2],
    end: [f32; 2],
    range: [f32; 2],
    coverage: [f32; 2],
    start_color: [f32; 4],
    end_color: [f32; 4],
) -> PaintLayer<'a> {
    PaintLayer {
        texture: PaintTexture::Borrowed(texture),
        source: unit_source(),
        bounds: rect,
        masks: masks.to_vec(),
        color: [1.0; 4],
        mode: TextureMode::Gradient {
            start,
            end,
            range,
            coverage,
            start_color,
            end_color,
        },
        sampling: TextureSampling::Nearest,
        wrap: TextureWrap::Edge,
    }
}

fn cached_backdrop_layer<'a>(
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

fn unit_source() -> TextureRect {
    TextureRect {
        x: 0.0,
        y: 0.0,
        width: 1.0,
        height: 1.0,
    }
}

fn color_rgba(color: u32, opacity: f32) -> [f32; 4] {
    let channel = |shift: u32| ((color >> shift) & 0xffu32) as f32 / 255.0 * opacity;
    [channel(16), channel(8), channel(0), channel(24)]
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::{backdrop_snapshot_rect, color_rgba, commands_may_touch, rounded_solid_border};
    use display_proto::{BorderStyle, ClipMask, CornerRadius, DisplayCommand, Rect};

    #[test]
    fn premultiplied_argb_lowers_to_rgba_constants() {
        assert_eq!(
            color_rgba(0x8040_2010, 0.5),
            [64.0 / 510.0, 32.0 / 510.0, 16.0 / 510.0, 128.0 / 510.0,]
        );
    }

    #[test]
    fn retained_clip_rejects_unrelated_effect_subtrees() {
        let damage = ClipMask {
            rect: Rect {
                x: 0,
                y: 0,
                width: 3008,
                height: 64,
            },
            radii: [CornerRadius::default(); 4],
        };
        let window_shadow = DisplayCommand::BoxShadow {
            rect: Rect {
                x: 300,
                y: 300,
                width: 900,
                height: 700,
            },
            radii: [CornerRadius::default(); 4],
            offset: [0.0, 24.0],
            blur: 32.0,
            spread: 0.0,
            color: 0x8000_0000,
            inset: false,
        };
        assert!(!commands_may_touch(&[window_shadow], &[damage], 3008, 1692));
        let topbar = DisplayCommand::SolidRect {
            rect: damage.rect,
            radii: [CornerRadius::default(); 4],
            color: 0xff00_0000,
        };
        assert!(commands_may_touch(&[topbar], &[damage], 3008, 1692));
    }

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
    fn uniform_rounded_solid_border_uses_one_analytic_ring() {
        let radii = [CornerRadius { x: 18, y: 18 }; 4];
        assert_eq!(
            rounded_solid_border(radii, [2.0; 4], [0xff35_c8ff; 4], [BorderStyle::Solid; 4],),
            Some((2.0, 0xff35_c8ff))
        );
        assert_eq!(
            rounded_solid_border(
                radii,
                [2.0, 1.0, 2.0, 1.0],
                [0xff35_c8ff; 4],
                [BorderStyle::Solid; 4],
            ),
            None
        );
    }
}
