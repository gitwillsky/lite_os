//! Lowering from validated display-list primitives to GPU draws.

use std::io;

use display_proto::{
    BorderStyle, ClipMask, DisplayCommand, DisplayListCommit, ImageSampling, Rect, TextureFormat,
    TextureRect,
};
use linux_uapi::drm::VirglResource;

use super::{GpuRenderer, TextureLayer, TextureMode, TextureSampling, TextureWrap};

struct PaintLayer<'a> {
    texture: &'a VirglResource,
    source: TextureRect,
    bounds: Rect,
    masks: Vec<ClipMask>,
    color: [f32; 4],
    mode: TextureMode,
    sampling: TextureSampling,
    wrap: TextureWrap,
}

impl GpuRenderer {
    /// Executes a complete immutable CSS display list in the compositor's sole
    /// VirGL context. Geometry assembly remains on the CPU; coverage, filtering,
    /// blending, blur and final rasterization execute in GPU shaders.
    pub fn render_display_list<'a>(
        &'a self,
        target: &VirglResource,
        list: DisplayListCommit<'_>,
        mut texture: impl FnMut(u32) -> Option<(&'a VirglResource, TextureFormat)>,
    ) -> io::Result<()> {
        self.render_display_list_filtered(target, list, &mut texture, None)
    }

    /// Renders the desktop snapshot without one movable window group.
    pub fn render_display_list_excluding<'a>(
        &'a self,
        target: &VirglResource,
        list: DisplayListCommit<'_>,
        mut texture: impl FnMut(u32) -> Option<(&'a VirglResource, TextureFormat)>,
        excluded_group: u32,
    ) -> io::Result<()> {
        self.render_display_list_filtered(target, list, &mut texture, Some(excluded_group))
    }

    fn render_display_list_filtered<'a>(
        &'a self,
        target: &VirglResource,
        list: DisplayListCommit<'_>,
        texture: &mut dyn FnMut(u32) -> Option<(&'a VirglResource, TextureFormat)>,
        excluded_group: Option<u32>,
    ) -> io::Result<()> {
        let commands = list.commands().collect::<Vec<_>>();
        self.render_commands(
            target,
            &commands,
            texture,
            excluded_group,
            Vec::new(),
            None,
            None,
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
        texture: &mut dyn FnMut(u32) -> Option<(&'a VirglResource, TextureFormat)>,
        excluded_group: Option<u32>,
        mut clip_stack: Vec<ClipMask>,
        mut group: Option<u32>,
        backdrop: Option<&VirglResource>,
    ) -> io::Result<()> {
        let mut layers = Vec::new();
        let mut target_initialized = false;
        let mut index = 0usize;
        while index < commands.len() {
            let command = commands[index];
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
                    self.flush(target, &mut layers, &mut target_initialized)?;
                    if !target_initialized {
                        self.render(target, &[])?;
                    }
                    let pop = matching_opacity_pop(commands, index)?;
                    let isolated_backdrop = self.snapshot_backdrop(target, backdrop)?;
                    let scratch = self
                        .context
                        .create_texture(target.width(), target.height())?;
                    self.render_commands(
                        &scratch,
                        &commands[index..pop],
                        texture,
                        excluded_group,
                        clip_stack.clone(),
                        group,
                        Some(&isolated_backdrop),
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
                            texture: &self.white,
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
                } => self.border_layers(
                    &mut layers,
                    &clip_stack,
                    rect,
                    radii,
                    widths,
                    colors,
                    styles,
                    1.0,
                )?,
                DisplayCommand::BoxShadow {
                    rect,
                    radii,
                    offset,
                    blur,
                    spread,
                    color,
                    inset,
                } => {
                    self.flush(target, &mut layers, &mut target_initialized)?;
                    self.render_shadow(
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
                        !target_initialized,
                    )?;
                    target_initialized = true;
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
                        texture: resource,
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
                        self.flush(target, &mut layers, &mut target_initialized)?;
                        let scratch = self
                            .context
                            .create_texture(target.width(), target.height())?;
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
                            texture: resource,
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
                    self.flush(target, &mut layers, &mut target_initialized)?;
                    if !target_initialized {
                        self.render(target, &[])?;
                    }
                    let scratch = self.snapshot_backdrop(target, backdrop)?;
                    let screen = Rect {
                        x: 0,
                        y: 0,
                        width: target.width(),
                        height: target.height(),
                    };
                    let masks = shape_masks(&clip_stack, rect, radii)?;
                    self.render_layers(
                        target,
                        &[TextureLayer {
                            texture: &scratch,
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
                    )?;
                    target_initialized = true;
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
            texture: &self.white,
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
    ) -> io::Result<VirglResource> {
        let screen = Rect {
            x: 0,
            y: 0,
            width: target.width(),
            height: target.height(),
        };
        let snapshot = self
            .context
            .create_texture(target.width(), target.height())?;
        if let Some(inherited) = inherited {
            self.render(
                &snapshot,
                &[TextureLayer {
                    texture: inherited,
                    source: texture_rect(screen),
                    bounds: screen,
                    clip: screen,
                    clip_masks: &[],
                    clip_offset: (0, 0),
                    color: [1.0; 4],
                    mode: TextureMode::Color,
                    sampling: TextureSampling::Linear,
                    wrap: TextureWrap::Edge,
                }],
            )?;
            self.render_layers(
                &snapshot,
                &[TextureLayer {
                    texture: target,
                    source: texture_rect(screen),
                    bounds: screen,
                    clip: screen,
                    clip_masks: &[],
                    clip_offset: (0, 0),
                    color: [1.0; 4],
                    mode: TextureMode::Color,
                    sampling: TextureSampling::Linear,
                    wrap: TextureWrap::Edge,
                }],
                false,
            )?;
        } else {
            self.render(
                &snapshot,
                &[TextureLayer {
                    texture: target,
                    source: texture_rect(screen),
                    bounds: screen,
                    clip: screen,
                    clip_masks: &[],
                    clip_offset: (0, 0),
                    color: [1.0; 4],
                    mode: TextureMode::Color,
                    sampling: TextureSampling::Linear,
                    wrap: TextureWrap::Edge,
                }],
            )?;
        }
        Ok(snapshot)
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
    fn render_shadow(
        &self,
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
        clear_target: bool,
    ) -> io::Result<()> {
        let distance = if inset { -spread } else { spread };
        let mask_rect = expand_rect(rect, distance, offset);
        if mask_rect.width == 0 || mask_rect.height == 0 {
            return Ok(());
        }
        let mask_radii = radii.map(|radius| display_proto::CornerRadius {
            x: (radius.x as f32 + distance).max(0.0).round() as u32,
            y: (radius.y as f32 + distance).max(0.0).round() as u32,
        });
        let scratch = self
            .context
            .create_texture(target.width(), target.height())?;
        let screen = Rect {
            x: 0,
            y: 0,
            width: target.width(),
            height: target.height(),
        };
        let mask = shape_masks(&[], mask_rect, mask_radii)?;
        self.render(
            &scratch,
            &[TextureLayer {
                texture: &self.white,
                source: unit_source(),
                bounds: mask_rect,
                clip: screen,
                clip_masks: &mask,
                clip_offset: (0, 0),
                color: [1.0; 4],
                mode: TextureMode::Color,
                sampling: TextureSampling::Nearest,
                wrap: TextureWrap::Edge,
            }],
        )?;
        let final_masks = if inset {
            shape_masks(clips, rect, radii)?
        } else {
            clips.to_vec()
        };
        self.render_layers(
            target,
            &[TextureLayer {
                texture: &scratch,
                source: texture_rect(screen),
                bounds: screen,
                clip: screen,
                clip_masks: &final_masks,
                clip_offset: (0, 0),
                color: color_rgba(color, opacity),
                mode: if inset {
                    TextureMode::InvertedMaskBlur { radius: blur }
                } else {
                    TextureMode::MaskBlur { radius: blur }
                },
                sampling: TextureSampling::Linear,
                wrap: TextureWrap::Edge,
            }],
            clear_target,
        )
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
                texture: layer.texture,
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
        texture,
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
    use super::color_rgba;

    #[test]
    fn premultiplied_argb_lowers_to_rgba_constants() {
        assert_eq!(
            color_rgba(0x8040_2010, 0.5),
            [64.0 / 510.0, 32.0 / 510.0, 16.0 / 510.0, 128.0 / 510.0,]
        );
    }
}
