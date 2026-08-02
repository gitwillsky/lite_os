//! VirGL scene renderer owned exclusively by the system compositor.

mod paint;

use std::io;

use display_proto::{ClipMask, ImageRepeat, MAX_NODE_CLIP_MASKS, Rect, TextureRect};
use linux_uapi::drm::{
    CommandEncoder, ObjectKind, SamplerFilter, SamplerWrap, ShaderStage, VirglContext,
    VirglResource,
};

const VERTEX_ELEMENTS: u32 = 1;
const VERTEX_SHADER: u32 = 2;
const FRAGMENT_SHADER: u32 = 3;
const BLEND: u32 = 4;
const DEPTH_STENCIL_ALPHA: u32 = 5;
const RASTERIZER: u32 = 6;
const SAMPLER_NEAREST_EDGE: u32 = 7;
const TARGET_SURFACE: u32 = 8;
const SOURCE_VIEW: u32 = 9;
const SAMPLER_LINEAR_EDGE: u32 = 10;
const SAMPLER_NEAREST_BACKGROUND: u32 = 11;
const SAMPLER_LINEAR_BACKGROUND: u32 = 15;
/// A primitive shape and its layer clip each contribute one mask in addition
/// to all protocol-level ancestor masks. Keeping clipping in this single
/// top-left logical space avoids host-rasterizer origin differences.
const MAX_GPU_CLIP_MASKS: usize = MAX_NODE_CLIP_MASKS + 2;
const CONSTANTS_PER_CLIP_MASK: usize = 6;
const COLOR_CONSTANT: usize = MAX_GPU_CLIP_MASKS * CONSTANTS_PER_CLIP_MASK;
const MODE_CONSTANT: usize = COLOR_CONSTANT + 1;
const PARAMETER_CONSTANT: usize = MODE_CONSTANT + 1;
const LAST_FRAGMENT_CONSTANT: usize = PARAMETER_CONSTANT + 3;

const VERTEX_SHADER_SOURCE: &str = "VERT\n\
DCL IN[0]\n\
DCL IN[1]\n\
DCL OUT[0], POSITION\n\
DCL OUT[1], GENERIC[0]\n\
DCL OUT[2], GENERIC[1]\n\
DCL CONST[0][0..2]\n\
IMM[0] FLT32 {0.0, 0.0, 0.0, 1.0}\n\
0: MAD OUT[0].xy, IN[0], CONST[0][0].zwzw, CONST[0][0].xyxy\n\
1: MOV OUT[0].zw, IMM[0].zzzw\n\
2: MAD OUT[1].xy, IN[1], CONST[0][2].zwzw, CONST[0][2].xyxy\n\
3: MOV OUT[1].zw, IMM[0].zzzw\n\
4: MAD OUT[2].xy, IN[0], CONST[0][1].zwzw, CONST[0][1].xyxy\n\
5: MOV OUT[2].zw, IMM[0].zzzw\n\
6: END\n";

#[repr(C)]
#[derive(Clone, Copy)]
struct Vertex {
    position: [f32; 2],
    texture: [f32; 2],
}

const QUAD: [Vertex; 6] = [
    Vertex {
        position: [0.0, 0.0],
        texture: [0.0, 0.0],
    },
    Vertex {
        position: [1.0, 0.0],
        texture: [1.0, 0.0],
    },
    Vertex {
        position: [1.0, 1.0],
        texture: [1.0, 1.0],
    },
    Vertex {
        position: [0.0, 0.0],
        texture: [0.0, 0.0],
    },
    Vertex {
        position: [1.0, 1.0],
        texture: [1.0, 1.0],
    },
    Vertex {
        position: [0.0, 1.0],
        texture: [0.0, 1.0],
    },
];

/// One texture draw in back-to-front order.
pub struct TextureLayer<'a> {
    pub texture: &'a VirglResource,
    pub source: TextureRect,
    pub bounds: Rect,
    pub clip: Rect,
    pub clip_masks: &'a [ClipMask],
    pub clip_offset: (i32, i32),
    pub color: [f32; 4],
    pub mode: TextureMode,
    pub sampling: TextureSampling,
    pub wrap: TextureWrap,
}

#[derive(Clone, Copy)]
pub enum TextureMode {
    Color,
    Mask,
    Gradient {
        start: [f32; 2],
        end: [f32; 2],
        range: [f32; 2],
        coverage: [f32; 2],
        start_color: [f32; 4],
        end_color: [f32; 4],
    },
    Blur {
        radius: f32,
    },
    MaskBlur {
        radius: f32,
    },
    InvertedMaskBlur {
        radius: f32,
    },
}

#[derive(Clone, Copy)]
pub enum TextureSampling {
    Nearest,
    Linear,
}

#[derive(Clone, Copy)]
pub enum TextureWrap {
    Edge,
    Background(ImageRepeat),
}

/// Persistent VirGL pipeline shared by every desktop and application surface.
pub struct GpuRenderer {
    context: VirglContext,
    vertices: VirglResource,
    white: VirglResource,
}

impl GpuRenderer {
    /// Creates and binds the one immutable compositor pipeline.
    pub fn new(context: &VirglContext) -> io::Result<Self> {
        let mut vertices = context.create_vertex_buffer(std::mem::size_of_val(&QUAD) as u32)?;
        let source = unsafe {
            std::slice::from_raw_parts(QUAD.as_ptr().cast::<u8>(), std::mem::size_of_val(&QUAD))
        };
        vertices.bytes_mut().copy_from_slice(source);
        vertices.transfer_buffer_to_host()?;
        let mut white = context.create_texture(1, 1)?;
        white.bytes_mut().copy_from_slice(&[255; 4]);
        white.transfer_to_host(0, 0, 1, 1)?;

        let mut command = CommandEncoder::new();
        command.create_textured_vertex_elements(VERTEX_ELEMENTS);
        command.create_shader(VERTEX_SHADER, ShaderStage::Vertex, VERTEX_SHADER_SOURCE);
        let fragment_shader = fragment_shader();
        command.create_shader(FRAGMENT_SHADER, ShaderStage::Fragment, &fragment_shader);
        command.create_premultiplied_blend(BLEND);
        command.create_depth_stencil_alpha(DEPTH_STENCIL_ALPHA);
        command.create_rasterizer(RASTERIZER);
        command.create_sampler_state(
            SAMPLER_NEAREST_EDGE,
            SamplerFilter::Nearest,
            SamplerWrap::ClampToEdge,
            SamplerWrap::ClampToEdge,
        );
        command.create_sampler_state(
            SAMPLER_LINEAR_EDGE,
            SamplerFilter::Linear,
            SamplerWrap::ClampToEdge,
            SamplerWrap::ClampToEdge,
        );
        for repeat in [
            ImageRepeat::NoRepeat,
            ImageRepeat::RepeatX,
            ImageRepeat::RepeatY,
            ImageRepeat::Repeat,
        ] {
            let (wrap_s, wrap_t) = background_wrap(repeat);
            command.create_sampler_state(
                SAMPLER_NEAREST_BACKGROUND + repeat as u32,
                SamplerFilter::Nearest,
                wrap_s,
                wrap_t,
            );
            command.create_sampler_state(
                SAMPLER_LINEAR_BACKGROUND + repeat as u32,
                SamplerFilter::Linear,
                wrap_s,
                wrap_t,
            );
        }
        command.bind_object(ObjectKind::VertexElements, VERTEX_ELEMENTS);
        command.bind_object(ObjectKind::Blend, BLEND);
        command.bind_object(ObjectKind::DepthStencilAlpha, DEPTH_STENCIL_ALPHA);
        command.bind_object(ObjectKind::Rasterizer, RASTERIZER);
        command.bind_shader(ShaderStage::Vertex, VERTEX_SHADER);
        command.bind_shader(ShaderStage::Fragment, FRAGMENT_SHADER);
        command.link_shaders(VERTEX_SHADER, FRAGMENT_SHADER);
        command.set_vertex_buffer(vertices.resource_id());
        context.exec(command.words(), &[&vertices])?;
        vertices.wait()?;
        white.wait()?;
        Ok(Self {
            context: context.clone(),
            vertices,
            white,
        })
    }

    /// Draws a complete ordered scene into one GPU render target.
    pub fn render(&self, target: &VirglResource, layers: &[TextureLayer<'_>]) -> io::Result<()> {
        self.render_layers(target, layers, true)
    }

    /// Draws ordered layers, optionally preserving the target's prior pixels.
    pub(super) fn render_layers(
        &self,
        target: &VirglResource,
        layers: &[TextureLayer<'_>],
        clear: bool,
    ) -> io::Result<()> {
        let screen = Rect {
            x: 0,
            y: 0,
            width: target.width(),
            height: target.height(),
        };
        let mut commands = CommandEncoder::new();
        commands.create_surface(TARGET_SURFACE, target.resource_id(), target.format());
        commands.set_framebuffer(TARGET_SURFACE);
        commands.set_viewport(target.width(), target.height());
        if clear {
            commands.clear([0.0, 0.0, 0.0, 0.0]);
        }
        self.context
            .exec(commands.words(), &[target, &self.vertices])?;

        let mut commands = CommandEncoder::new();
        let mut resources = vec![target, &self.vertices];
        for layer in layers {
            let clip = layer.clip_masks.iter().fold(layer.clip, |clip, mask| {
                intersect(clip, translated(mask.rect, layer.clip_offset)).unwrap_or_default()
            });
            let Some(clip) = intersect(clip, screen) else {
                continue;
            };
            if layer.bounds.width == 0 || layer.bounds.height == 0 {
                continue;
            }
            commands.create_sampler_view(
                SOURCE_VIEW,
                layer.texture.resource_id(),
                layer.texture.format(),
            );
            let sampler = sampler_handle(layer.sampling, layer.wrap);
            commands.set_texture(SOURCE_VIEW, sampler);
            commands.set_constants(
                ShaderStage::Vertex,
                &transform(
                    layer.bounds,
                    layer.source,
                    layer.texture.width(),
                    layer.texture.height(),
                    target.width(),
                    target.height(),
                ),
            );
            let mut fragment = clip_constants(layer.clip_masks, layer.clip_offset, clip, screen);
            fragment.extend(layer.color.map(f32::to_bits));
            let (mode, parameters) = fragment_parameters(layer.mode, layer.texture);
            fragment.extend([mode.to_bits(), 0, 0, 0]);
            fragment.extend(parameters);
            commands.set_constants(ShaderStage::Fragment, &fragment);
            commands.draw_triangles(0, 6);
            commands.destroy_object(ObjectKind::SamplerView, SOURCE_VIEW);
            resources.push(layer.texture);
            if commands.len() >= 12_000 {
                self.context.exec(commands.words(), &resources)?;
                commands = CommandEncoder::new();
                resources.clear();
                resources.extend([target, &self.vertices]);
            }
        }
        if !commands.is_empty() {
            self.context.exec(commands.words(), &resources)?;
        }
        let mut commands = CommandEncoder::new();
        commands.destroy_object(ObjectKind::Surface, TARGET_SURFACE);
        self.context.exec(commands.words(), &[target])?;
        target.wait()
    }
}

fn transform(
    bounds: Rect,
    source: TextureRect,
    texture_width: u32,
    texture_height: u32,
    width: u32,
    height: u32,
) -> [u32; 12] {
    let width = width as f32;
    let height = height as f32;
    // Y_0_TOP resources store the first guest row at v=1: a quad's top edge
    // samples v=(texture_height-source.y)/texture_height, not the mirrored
    // band. Full textures need the same mapping to preserve top-to-bottom
    // orientation; partial glyph-atlas bands make a mismatch immediately
    // visible as content from another row.
    // VirGL normalizes the protocol's negative-Y viewport to a positive GL
    // viewport. Destination positions therefore use bottom-left NDC; applying
    // another Y inversion here mirrors every non-fullscreen layer to H-y-h.
    [
        (-1.0 + 2.0 * bounds.x as f32 / width).to_bits(),
        (-1.0 + 2.0 * bounds.y as f32 / height).to_bits(),
        (2.0 * bounds.width as f32 / width).to_bits(),
        (2.0 * bounds.height as f32 / height).to_bits(),
        (bounds.x as f32).to_bits(),
        (bounds.y as f32).to_bits(),
        (bounds.width as f32).to_bits(),
        (bounds.height as f32).to_bits(),
        (source.x / texture_width as f32).to_bits(),
        ((texture_height as f32 - source.y) / texture_height as f32).to_bits(),
        (source.width / texture_width as f32).to_bits(),
        (-source.height / texture_height as f32).to_bits(),
    ]
}

fn sampler_handle(sampling: TextureSampling, wrap: TextureWrap) -> u32 {
    match (sampling, wrap) {
        (TextureSampling::Nearest, TextureWrap::Edge) => SAMPLER_NEAREST_EDGE,
        (TextureSampling::Linear, TextureWrap::Edge) => SAMPLER_LINEAR_EDGE,
        (TextureSampling::Nearest, TextureWrap::Background(repeat)) => {
            SAMPLER_NEAREST_BACKGROUND + repeat as u32
        }
        (TextureSampling::Linear, TextureWrap::Background(repeat)) => {
            SAMPLER_LINEAR_BACKGROUND + repeat as u32
        }
    }
}

fn background_wrap(repeat: ImageRepeat) -> (SamplerWrap, SamplerWrap) {
    let border = SamplerWrap::ClampToBorder;
    let repeating = SamplerWrap::Repeat;
    match repeat {
        ImageRepeat::NoRepeat => (border, border),
        ImageRepeat::RepeatX => (repeating, border),
        ImageRepeat::RepeatY => (border, repeating),
        ImageRepeat::Repeat => (repeating, repeating),
    }
}

fn clip_constants(
    masks: &[ClipMask],
    offset: (i32, i32),
    layer_clip: Rect,
    screen: Rect,
) -> Vec<u32> {
    let mut constants = Vec::with_capacity(MAX_GPU_CLIP_MASKS * 24);
    for index in 0..MAX_GPU_CLIP_MASKS {
        let mask = masks.get(index).copied();
        let (rect, radii) = match mask {
            Some(mask) => (translated(mask.rect, offset), mask.radii),
            None if index == masks.len() => (layer_clip, Default::default()),
            None => (screen, Default::default()),
        };
        let x1 = rect.x as f32;
        let y1 = rect.y as f32;
        let x2 = rect.x.saturating_add_unsigned(rect.width) as f32;
        let y2 = rect.y.saturating_add_unsigned(rect.height) as f32;
        let centers = [
            (x1, y1, 1.0, 1.0),
            (x2, y1, -1.0, 1.0),
            (x2, y2, -1.0, -1.0),
            (x1, y2, 1.0, -1.0),
        ];
        let mut scales = [0.0f32; 4];
        for (corner, ((anchor_x, anchor_y, direction_x, direction_y), radius)) in
            centers.into_iter().zip(radii).enumerate()
        {
            let radius_x = radius.x as f32;
            let radius_y = radius.y as f32;
            let center_x = anchor_x + direction_x * radius_x;
            let center_y = anchor_y + direction_y * radius_y;
            constants.extend([
                center_x.to_bits(),
                center_y.to_bits(),
                radius_x.max(1.0).recip().to_bits(),
                radius_y.max(1.0).recip().to_bits(),
            ]);
            scales[corner] = radius_x.min(radius_y) * 0.5;
        }
        constants.extend(scales.map(f32::to_bits));
        constants.extend([x1.to_bits(), y1.to_bits(), x2.to_bits(), y2.to_bits()]);
    }
    constants
}

fn fragment_parameters(mode: TextureMode, texture: &VirglResource) -> (f32, [u32; 16]) {
    let mut words = [0; 16];
    match mode {
        TextureMode::Color => (0.0, words),
        TextureMode::Mask => (1.0, words),
        TextureMode::Gradient {
            start,
            end,
            range,
            coverage,
            start_color,
            end_color,
        } => {
            words[..4].copy_from_slice(&[
                start[0].to_bits(),
                start[1].to_bits(),
                end[0].to_bits(),
                end[1].to_bits(),
            ]);
            words[4..8].copy_from_slice(&[
                range[0].to_bits(),
                range[1].to_bits(),
                coverage[0].to_bits(),
                coverage[1].to_bits(),
            ]);
            words[8..12].copy_from_slice(&start_color.map(f32::to_bits));
            words[12..16].copy_from_slice(&end_color.map(f32::to_bits));
            (2.0, words)
        }
        TextureMode::Blur { radius } => {
            words[0] = (radius / texture.width() as f32).to_bits();
            words[1] = (radius / texture.height() as f32).to_bits();
            (3.0, words)
        }
        TextureMode::MaskBlur { radius } => {
            words[0] = (radius / texture.width() as f32).to_bits();
            words[1] = (radius / texture.height() as f32).to_bits();
            (4.0, words)
        }
        TextureMode::InvertedMaskBlur { radius } => {
            words[0] = (radius / texture.width() as f32).to_bits();
            words[1] = (radius / texture.height() as f32).to_bits();
            (5.0, words)
        }
    }
}

fn fragment_shader() -> String {
    use std::fmt::Write as _;

    let mut shader = String::from(&format!(
        "FRAG\n\
DCL IN[0], GENERIC[0], LINEAR\n\
DCL IN[1], GENERIC[1], LINEAR\n\
DCL OUT[0], COLOR\n\
DCL SAMP[0]\n\
DCL SVIEW[0], 2D, FLOAT\n\
DCL TEMP[0..7]\n\
DCL CONST[0][0..{LAST_FRAGMENT_CONSTANT}]\n\
IMM[0] FLT32 {{0.5, 1.0, 0.0, 0.11111111}}\n\
IMM[1] FLT32 {{1.0, 2.0, 3.0, 4.0}}\n\
IMM[2] FLT32 {{-1.0, -1.0, 0.0, 0.0}}\n\
IMM[3] FLT32 {{0.0, -1.0, 0.0, 0.0}}\n\
IMM[4] FLT32 {{1.0, -1.0, 0.0, 0.0}}\n\
IMM[5] FLT32 {{-1.0, 0.0, 0.0, 0.0}}\n\
IMM[6] FLT32 {{0.0, 0.0, 0.0, 0.0}}\n\
IMM[7] FLT32 {{1.0, 0.0, 0.0, 0.0}}\n\
IMM[8] FLT32 {{-1.0, 1.0, 0.0, 0.0}}\n\
IMM[9] FLT32 {{0.0, 1.0, 0.0, 0.0}}\n\
IMM[10] FLT32 {{1.0, 1.0, 0.0, 0.0}}\n\
IMM[11] FLT32 {{5.0, 0.0, 0.0, 0.0}}\n"
    ));
    let mut instruction = 0;
    writeln!(shader, "{instruction}: MOV TEMP[3], IMM[0].yyyy").unwrap();
    instruction += 1;
    for mask in 0..MAX_GPU_CLIP_MASKS {
        let base = mask * CONSTANTS_PER_CLIP_MASK;
        let edges = base + 5;
        writeln!(
            shader,
            "{instruction}: SGE TEMP[1].x, IN[1].xxxx, CONST[0][{edges}].xxxx"
        )
        .unwrap();
        instruction += 1;
        writeln!(
            shader,
            "{instruction}: SLT TEMP[1].y, IN[1].xxxx, CONST[0][{edges}].zzzz"
        )
        .unwrap();
        instruction += 1;
        writeln!(
            shader,
            "{instruction}: MUL TEMP[1].x, TEMP[1].xxxx, TEMP[1].yyyy"
        )
        .unwrap();
        instruction += 1;
        writeln!(
            shader,
            "{instruction}: SGE TEMP[1].y, IN[1].yyyy, CONST[0][{edges}].yyyy"
        )
        .unwrap();
        instruction += 1;
        writeln!(
            shader,
            "{instruction}: MUL TEMP[1].x, TEMP[1].xxxx, TEMP[1].yyyy"
        )
        .unwrap();
        instruction += 1;
        writeln!(
            shader,
            "{instruction}: SLT TEMP[1].y, IN[1].yyyy, CONST[0][{edges}].wwww"
        )
        .unwrap();
        instruction += 1;
        writeln!(
            shader,
            "{instruction}: MUL TEMP[1].x, TEMP[1].xxxx, TEMP[1].yyyy"
        )
        .unwrap();
        instruction += 1;
        writeln!(
            shader,
            "{instruction}: MUL TEMP[3].x, TEMP[3].xxxx, TEMP[1].xxxx"
        )
        .unwrap();
        instruction += 1;
        for corner in 0..4 {
            let x_comparison = if corner == 0 || corner == 3 {
                format!(
                    "SLT TEMP[1].x, IN[1].xxxx, CONST[0][{}].xxxx",
                    base + corner
                )
            } else {
                format!(
                    "SLT TEMP[1].x, CONST[0][{}].xxxx, IN[1].xxxx",
                    base + corner
                )
            };
            let y_comparison = if corner <= 1 {
                format!(
                    "SLT TEMP[1].y, IN[1].yyyy, CONST[0][{}].yyyy",
                    base + corner
                )
            } else {
                format!(
                    "SLT TEMP[1].y, CONST[0][{}].yyyy, IN[1].yyyy",
                    base + corner
                )
            };
            writeln!(shader, "{instruction}: {x_comparison}").unwrap();
            instruction += 1;
            writeln!(shader, "{instruction}: {y_comparison}").unwrap();
            instruction += 1;
            writeln!(
                shader,
                "{instruction}: MUL TEMP[1].x, TEMP[1].xxxx, TEMP[1].yyyy"
            )
            .unwrap();
            instruction += 1;
            writeln!(shader, "{instruction}: IF TEMP[1].xxxx").unwrap();
            instruction += 1;
            writeln!(
                shader,
                "{instruction}: ADD TEMP[2].xy, IN[1].xyxy, -CONST[0][{}].xyxy",
                base + corner
            )
            .unwrap();
            instruction += 1;
            writeln!(
                shader,
                "{instruction}: MUL TEMP[2].xy, TEMP[2].xyxy, CONST[0][{}].zwzw",
                base + corner
            )
            .unwrap();
            instruction += 1;
            writeln!(shader, "{instruction}: DP2 TEMP[2].x, TEMP[2], TEMP[2]").unwrap();
            instruction += 1;
            let component = ["xxxx", "yyyy", "zzzz", "wwww"][corner];
            writeln!(
                shader,
                "{instruction}: ADD TEMP[2].x, IMM[0].yyyy, -TEMP[2].xxxx"
            )
            .unwrap();
            instruction += 1;
            writeln!(
                shader,
                "{instruction}: MAD TEMP[2].x, TEMP[2].xxxx, CONST[0][{}].{}, IMM[0].xxxx",
                base + 4,
                component
            )
            .unwrap();
            instruction += 1;
            writeln!(
                shader,
                "{instruction}: MIN TEMP[3].x, TEMP[3].xxxx, TEMP[2].xxxx"
            )
            .unwrap();
            instruction += 1;
            writeln!(shader, "{instruction}: ENDIF").unwrap();
            instruction += 1;
        }
    }
    writeln!(
        shader,
        "{instruction}: MAX TEMP[3].x, TEMP[3].xxxx, IMM[0].zzzz"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: SEQ TEMP[7].x, CONST[0][{MODE_CONSTANT}].xxxx, IMM[1].yyyy"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: IF TEMP[7].xxxx").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ADD TEMP[4].xy, CONST[0][{PARAMETER_CONSTANT}].zwzw, -CONST[0][{PARAMETER_CONSTANT}].xyxy").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: ADD TEMP[5].xy, IN[1].xyxy, -CONST[0][{PARAMETER_CONSTANT}].xyxy"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: DP2 TEMP[5].x, TEMP[5], TEMP[4]").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: DP2 TEMP[5].y, TEMP[4], TEMP[4]").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: RCP TEMP[5].y, TEMP[5].yyyy").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MUL TEMP[5].x, TEMP[5].xxxx, TEMP[5].yyyy"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: MOV TEMP[5].z, TEMP[5].xxxx").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: SGE TEMP[7].y, CONST[0][{}].zzzz, TEMP[5].zzzz",
        PARAMETER_CONSTANT + 1
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: SLT TEMP[7].z, CONST[0][{}].wwww, TEMP[5].zzzz",
        PARAMETER_CONSTANT + 1
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MAX TEMP[7].y, TEMP[7].yyyy, TEMP[7].zzzz"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: ADD TEMP[7].y, IMM[0].yyyy, -TEMP[7].yyyy"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: ADD TEMP[5].x, TEMP[5].xxxx, -CONST[0][{}].xxxx",
        PARAMETER_CONSTANT + 1
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: ADD TEMP[5].y, CONST[0][{}].yyyy, -CONST[0][{}].xxxx",
        PARAMETER_CONSTANT + 1,
        PARAMETER_CONSTANT + 1
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: RCP TEMP[5].y, TEMP[5].yyyy").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MUL TEMP[5].x, TEMP[5].xxxx, TEMP[5].yyyy"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MAX TEMP[5].x, TEMP[5].xxxx, IMM[0].zzzz"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MIN TEMP[5].x, TEMP[5].xxxx, IMM[0].yyyy"
    )
    .unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: LRP TEMP[0], TEMP[5].xxxx, CONST[0][{}], CONST[0][{}]",
        PARAMETER_CONSTANT + 3,
        PARAMETER_CONSTANT + 2
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: MUL TEMP[0], TEMP[0], TEMP[7].yyyy").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ELSE").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: SGE TEMP[7].x, CONST[0][{MODE_CONSTANT}].xxxx, IMM[1].zzzz"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: IF TEMP[7].xxxx").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: MOV TEMP[0], IMM[0].zzzz").unwrap();
    instruction += 1;
    for offset in 2..=10 {
        writeln!(shader, "{instruction}: MAD TEMP[6].xy, CONST[0][{PARAMETER_CONSTANT}].xyxy, IMM[{offset}].xyxy, IN[0].xyxy").unwrap();
        instruction += 1;
        writeln!(shader, "{instruction}: TEX TEMP[4], TEMP[6], SAMP[0], 2D").unwrap();
        instruction += 1;
        writeln!(
            shader,
            "{instruction}: MAD TEMP[0], TEMP[4], IMM[0].wwww, TEMP[0]"
        )
        .unwrap();
        instruction += 1;
    }
    writeln!(
        shader,
        "{instruction}: SEQ TEMP[7].x, CONST[0][{MODE_CONSTANT}].xxxx, IMM[11].xxxx"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: IF TEMP[7].xxxx").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: ADD TEMP[0].w, IMM[0].yyyy, -TEMP[0].wwww"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ENDIF").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: SGE TEMP[7].x, CONST[0][{MODE_CONSTANT}].xxxx, IMM[1].wwww"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: IF TEMP[7].xxxx").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MUL TEMP[0], CONST[0][{COLOR_CONSTANT}], TEMP[0].wwww"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ELSE").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MUL TEMP[0], TEMP[0], CONST[0][{COLOR_CONSTANT}]"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ENDIF").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ELSE").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: TEX TEMP[0], IN[0], SAMP[0], 2D").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: SEQ TEMP[7].x, CONST[0][{MODE_CONSTANT}].xxxx, IMM[1].xxxx"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: IF TEMP[7].xxxx").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MUL TEMP[0], CONST[0][{COLOR_CONSTANT}], TEMP[0].xxxx"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ELSE").unwrap();
    instruction += 1;
    writeln!(
        shader,
        "{instruction}: MUL TEMP[0], TEMP[0], CONST[0][{COLOR_CONSTANT}]"
    )
    .unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ENDIF").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ENDIF").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: ENDIF").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: MUL OUT[0], TEMP[0], TEMP[3].xxxx").unwrap();
    instruction += 1;
    writeln!(shader, "{instruction}: END").unwrap();
    shader
}

fn translated(rectangle: Rect, offset: (i32, i32)) -> Rect {
    Rect {
        x: rectangle.x.saturating_add(offset.0),
        y: rectangle.y.saturating_add(offset.1),
        ..rectangle
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_left_bounds_and_y0_top_texture_lower_together() {
        let words = transform(
            Rect {
                x: 50,
                y: 25,
                width: 100,
                height: 50,
            },
            TextureRect {
                x: 0.0,
                y: 0.0,
                width: 40.0,
                height: 20.0,
            },
            80,
            40,
            200,
            100,
        );
        assert_eq!(
            words.map(f32::from_bits),
            [
                -0.5, -0.5, 1.0, 1.0, 50.0, 25.0, 100.0, 50.0, 0.0, 1.0, 0.5, -0.5,
            ]
        );
    }

    #[test]
    fn partial_texture_source_samples_its_own_band() {
        // A glyph-strip source (sy=10, sh=5) inside a 40-row atlas must map
        // the quad top edge to v=(40-10)/40 and the bottom edge to
        // v=(40-10-5)/40; the mirrored band (v=15/40..10/40) samples the
        // rows of a different glyph.
        let words = transform(
            Rect {
                x: 0,
                y: 0,
                width: 8,
                height: 5,
            },
            TextureRect {
                x: 4.0,
                y: 10.0,
                width: 8.0,
                height: 5.0,
            },
            2048,
            40,
            100,
            100,
        );
        assert_eq!(
            words.map(f32::from_bits)[8..],
            [4.0 / 2048.0, 1.0 - 10.0 / 40.0, 8.0 / 2048.0, -5.0 / 40.0,]
        );
    }

    #[test]
    fn layer_clip_is_encoded_after_ancestor_masks() {
        let clip = Rect {
            x: 50,
            y: 10,
            width: 100,
            height: 20,
        };
        let constants = clip_constants(&[], (0, 0), clip, Rect::default());
        assert_eq!(f32::from_bits(constants[0]), 50.0);
        assert_eq!(f32::from_bits(constants[1]), 10.0);
        assert_eq!(f32::from_bits(constants[4]), 150.0);
        assert_eq!(f32::from_bits(constants[9]), 30.0);
        assert_eq!(
            [constants[20], constants[21], constants[22], constants[23],].map(f32::from_bits),
            [50.0, 10.0, 150.0, 30.0]
        );
    }

    #[test]
    fn fragment_shader_contains_every_protocol_clip_mask() {
        let shader = fragment_shader();
        assert!(shader.contains(&format!("CONST[0][0..{LAST_FRAGMENT_CONSTANT}]")));
        assert_eq!(
            shader.matches(": IF TEMP[1].xxxx").count(),
            MAX_GPU_CLIP_MASKS * 4
        );
        assert_eq!(
            shader.matches("SLT TEMP[1].y, IN[1].xxxx").count(),
            MAX_GPU_CLIP_MASKS
        );
    }
}
