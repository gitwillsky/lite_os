//! VirGL scene renderer owned exclusively by the system compositor.

mod paint;
mod shader;

use std::{cell::RefCell, io, rc::Rc};

use display_proto::{
    ClipMask, ImageRepeat, MAX_DISPLAY_STACK_DEPTH, MAX_NODE_CLIP_MASKS, Rect, TextureRect,
};
use shader::{VERTEX_SHADER_SOURCE, fragment_shader};
use linux_uapi::drm::{
    CommandEncoder, ObjectKind, SamplerFilter, SamplerWrap, ShaderStage, VirglContext,
    VirglResource,
};

const VERTEX_ELEMENTS: u32 = 1;
const VERTEX_SHADER: u32 = 2;
const ROUNDED_FRAGMENT_SHADER: u32 = 3;
const BLEND: u32 = 4;
const REPLACE_BLEND: u32 = 19;
const FLAT_FRAGMENT_SHADER: u32 = 20;
const DEPTH_STENCIL_ALPHA: u32 = 5;
const RASTERIZER: u32 = 6;
const SAMPLER_NEAREST_EDGE: u32 = 7;
const TARGET_SURFACE: u32 = 8;
const SOURCE_VIEW: u32 = 9;
const SAMPLER_LINEAR_EDGE: u32 = 10;
const SAMPLER_NEAREST_BACKGROUND: u32 = 11;
const SAMPLER_LINEAR_BACKGROUND: u32 = 15;
/// A rounded primitive can contribute one mask in addition to all
/// protocol-level rounded ancestor masks. Rectangular masks are enforced by
/// cropping the submitted quad, so sending them through this per-fragment path
/// would make every damaged pixel repeat work already completed on the CPU.
const MAX_GPU_CLIP_MASKS: usize = MAX_NODE_CLIP_MASKS + 1;
const CONSTANTS_PER_CLIP_MASK: usize = 6;
// Each nested opacity group retains one backdrop and one isolated target while rendering its
// child slice. The deepest blur additionally owns its clean source plus the reduced and horizontal
// Gaussian passes. The display protocol caps opacity nesting at 16, so this capacity covers every
// valid simultaneous effect lifetime without resource churn in steady-state paint.
const EFFECT_TARGET_CAPACITY: usize = display_proto::MAX_DISPLAY_STACK_DEPTH * 2 + 3;
const BACKDROP_CACHE_CAPACITY: usize = MAX_DISPLAY_STACK_DEPTH * 2 + 1;

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
    Gaussian {
        texel_step: [f32; 2],
    },
    MaskGaussian {
        texel_step: [f32; 2],
    },
    Shadow {
        mask: Rect,
        radii: [display_proto::CornerRadius; 4],
        support: f32,
        spread: f32,
        offset: [f32; 2],
        inset: bool,
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

struct EffectScratch<'a> {
    pool: &'a RefCell<Vec<VirglResource>>,
    resource: Option<VirglResource>,
}

struct BackdropCacheEntry {
    owner: (u64, u32),
    command_slot: usize,
    prefix: Vec<u8>,
    texture: Rc<VirglResource>,
}

impl std::ops::Deref for EffectScratch<'_> {
    type Target = VirglResource;

    fn deref(&self) -> &Self::Target {
        self.resource
            .as_ref()
            .expect("live effect scratch lost its resource")
    }
}

impl Drop for EffectScratch<'_> {
    fn drop(&mut self) {
        let resource = self
            .resource
            .take()
            .expect("effect scratch resource recycled twice");
        let mut pool = self.pool.borrow_mut();
        if pool.len() == EFFECT_TARGET_CAPACITY {
            // A mode/geometry change can fill the pool with obsolete extents. Retire exactly one
            // old target so the current geometry becomes reusable; steady-state paint never
            // reaches this branch and therefore never GEM_CLOSEs an effect target.
            pool.swap_remove(0);
        }
        pool.push(resource);
    }
}

/// Persistent VirGL pipeline shared by every desktop and application surface.
pub struct GpuRenderer {
    context: VirglContext,
    vertices: VirglResource,
    white: VirglResource,
    /// Single-thread-owned fixed-capacity effect targets shared by opacity and texture blur.
    /// Without this sole pool, every repaint synchronously creates/attaches/unrefs several host
    /// resources and blocks evdev routing for hundreds of milliseconds.
    effect_scratch: RefCell<Vec<VirglResource>>,
    /// Exact-prefix cache for stable CSS backdrops, bounded independently from
    /// client command count. Without it, scrolling a child reruns every static
    /// ancestor blur even though no command before that blur changed.
    backdrop_cache: RefCell<Vec<BackdropCacheEntry>>,
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
        let rounded_fragment_shader = fragment_shader(MAX_GPU_CLIP_MASKS);
        command.create_shader(
            ROUNDED_FRAGMENT_SHADER,
            ShaderStage::Fragment,
            &rounded_fragment_shader,
        );
        let flat_fragment_shader = fragment_shader(0);
        command.create_shader(
            FLAT_FRAGMENT_SHADER,
            ShaderStage::Fragment,
            &flat_fragment_shader,
        );
        command.create_premultiplied_blend(BLEND);
        command.create_replace_blend(REPLACE_BLEND);
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
        command.bind_shader(ShaderStage::Fragment, ROUNDED_FRAGMENT_SHADER);
        command.link_shaders(VERTEX_SHADER, ROUNDED_FRAGMENT_SHADER);
        command.link_shaders(VERTEX_SHADER, FLAT_FRAGMENT_SHADER);
        command.set_vertex_buffer(vertices.resource_id());
        context.exec(command.words(), &[&vertices])?;
        vertices.wait()?;
        white.wait()?;
        let mut effect_scratch = Vec::new();
        effect_scratch
            .try_reserve_exact(EFFECT_TARGET_CAPACITY)
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        let mut backdrop_cache = Vec::new();
        backdrop_cache
            .try_reserve_exact(BACKDROP_CACHE_CAPACITY)
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        Ok(Self {
            context: context.clone(),
            vertices,
            white,
            effect_scratch: RefCell::new(effect_scratch),
            backdrop_cache: RefCell::new(backdrop_cache),
        })
    }

    fn take_effect_scratch(&self, width: u32, height: u32) -> io::Result<EffectScratch<'_>> {
        let resource = {
            let mut pool = self.effect_scratch.borrow_mut();
            pool.iter()
                .position(|target| target.width() == width && target.height() == height)
                .map(|index| pool.swap_remove(index))
        };
        Ok(EffectScratch {
            pool: &self.effect_scratch,
            resource: Some(match resource {
                Some(resource) => resource,
                None => self.context.create_texture(width, height)?,
            }),
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
        self.render_layers_with_blend(target, layers, clear, None)
    }

    fn render_layers_with_blend(
        &self,
        target: &VirglResource,
        layers: &[TextureLayer<'_>],
        clear: bool,
        blend: Option<u32>,
    ) -> io::Result<()> {
        let screen = Rect {
            x: 0,
            y: 0,
            width: target.width(),
            height: target.height(),
        };
        let mut commands = CommandEncoder::new();
        if let Some(blend) = blend {
            commands.bind_object(ObjectKind::Blend, blend);
        }
        commands.create_surface(TARGET_SURFACE, target.resource_id(), target.format());
        commands.set_framebuffer(TARGET_SURFACE);
        commands.set_viewport(target.width(), target.height());
        if clear {
            commands.clear([0.0, 0.0, 0.0, 0.0]);
        }
        let mut resources = vec![target, &self.vertices];
        let mut bound_fragment_shader = None;
        for layer in layers {
            let clip = layer.clip_masks.iter().fold(layer.clip, |clip, mask| {
                intersect(clip, translated(mask.rect, layer.clip_offset)).unwrap_or_default()
            });
            let Some(clip) = intersect(clip, screen) else {
                continue;
            };
            // 1. Every mask's rectangular extent is enforced by this crop.
            // 2. Only curved corner coverage remains for the fragment shader;
            // submitting rectangular masks there would repeat their edge tests
            // for every pixel and made scroll/resize slower than CPU rendering.
            // 3. Crop the affine texture source with the raster bounds so the
            // optimization cannot change sampling inside the visible region.
            let Some((bounds, source)) = clipped_layer(layer.bounds, layer.source, clip) else {
                continue;
            };
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
                    bounds,
                    source,
                    layer.texture.width(),
                    layer.texture.height(),
                    target.width(),
                    target.height(),
                ),
            );
            let active_clip_count = rounded_clip_count(
                layer.clip_masks,
                layer.clip_offset,
                bounds,
            );
            let fragment_shader = if active_clip_count == 0 {
                FLAT_FRAGMENT_SHADER
            } else {
                ROUNDED_FRAGMENT_SHADER
            };
            if bound_fragment_shader != Some(fragment_shader) {
                commands.bind_shader(ShaderStage::Fragment, fragment_shader);
                bound_fragment_shader = Some(fragment_shader);
            }
            let mut fragment = if active_clip_count == 0 {
                Vec::with_capacity(24)
            } else {
                clip_constants(
                    layer.clip_masks,
                    layer.clip_offset,
                    bounds,
                    screen,
                )
            };
            fragment.extend(layer.color.map(f32::to_bits));
            let (mode, parameters) = fragment_parameters(layer.mode, layer.texture);
            fragment.extend([mode.to_bits(), (active_clip_count as f32).to_bits(), 0, 0]);
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
        commands.destroy_object(ObjectKind::Surface, TARGET_SURFACE);
        if blend.is_some() {
            commands.bind_object(ObjectKind::Blend, BLEND);
        }
        self.context.exec(commands.words(), &resources)
    }

    /// Seeds a retained target from the exact previous revision and clears the
    /// rectangle that the new display list will replace.
    pub(super) fn prepare_retained_target(
        &self,
        target: &VirglResource,
        base: &VirglResource,
        repair: Option<Rect>,
        damage: Rect,
    ) -> io::Result<()> {
        if target.width() != base.width()
            || target.height() != base.height()
            || target.format() != base.format()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "retained base texture geometry or format mismatch",
            ));
        }
        let screen = Rect {
            x: 0,
            y: 0,
            width: target.width(),
            height: target.height(),
        };
        let mut replacements = Vec::with_capacity(2);
        if let Some(repair) = repair {
            replacements.push(TextureLayer {
                    texture: base,
                    source: TextureRect {
                        x: 0.0,
                        y: 0.0,
                        width: base.width() as f32,
                        height: base.height() as f32,
                    },
                    bounds: screen,
                    clip: repair,
                    clip_masks: &[],
                    clip_offset: (0, 0),
                    color: [1.0; 4],
                    mode: TextureMode::Color,
                    sampling: TextureSampling::Nearest,
                    wrap: TextureWrap::Edge,
                });
        }
        if damage.width != 0 && damage.height != 0 {
            replacements.push(TextureLayer {
                texture: &self.white,
                source: TextureRect {
                    x: 0.0,
                    y: 0.0,
                    width: 1.0,
                    height: 1.0,
                },
                bounds: damage,
                clip: damage,
                clip_masks: &[],
                clip_offset: (0, 0),
                color: [0.0; 4],
                mode: TextureMode::Color,
                sampling: TextureSampling::Nearest,
                wrap: TextureWrap::Edge,
            });
        }
        if replacements.is_empty() {
            return Ok(());
        }
        // Repair must precede the transparent replacement when the rectangles
        // overlap. One REPLACE batch preserves that order while avoiding a
        // second VirtIO control-queue submission on every retained frame.
        self.render_layers_with_blend(
            target,
            &replacements,
            false,
            Some(REPLACE_BLEND),
        )
    }

    /// Replaces one target rectangle with transparent pixels before a clipped
    /// scene replay. Without replacement, source-over would accumulate the old
    /// translucent window, text, and backdrop pixels on every partial frame.
    pub(super) fn clear_damage(&self, target: &VirglResource, damage: Rect) -> io::Result<()> {
        let Some(damage) = intersect(
            damage,
            Rect {
                x: 0,
                y: 0,
                width: target.width(),
                height: target.height(),
            },
        ) else {
            return Ok(());
        };
        self.render_layers_with_blend(
            target,
            &[TextureLayer {
                texture: &self.white,
                source: TextureRect {
                    x: 0.0,
                    y: 0.0,
                    width: 1.0,
                    height: 1.0,
                },
                bounds: damage,
                clip: damage,
                clip_masks: &[],
                clip_offset: (0, 0),
                color: [0.0; 4],
                mode: TextureMode::Color,
                sampling: TextureSampling::Nearest,
                wrap: TextureWrap::Edge,
            }],
            false,
            Some(REPLACE_BLEND),
        )
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
    bounds: Rect,
    screen: Rect,
) -> Vec<u32> {
    let mut constants = Vec::with_capacity(MAX_GPU_CLIP_MASKS * 24);
    let mut rounded = masks
        .iter()
        .copied()
        .filter(|mask| rounded_mask_affects(*mask, offset, bounds));
    for _ in 0..MAX_GPU_CLIP_MASKS {
        let (rect, radii) = match rounded.next() {
            Some(mask) => (translated(mask.rect, offset), mask.radii),
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

fn rounded_clip_count(masks: &[ClipMask], offset: (i32, i32), bounds: Rect) -> usize {
    masks
        .iter()
        .copied()
        .filter(|mask| rounded_mask_affects(*mask, offset, bounds))
        .count()
}

fn rounded_mask_affects(mask: ClipMask, offset: (i32, i32), bounds: Rect) -> bool {
    let rect = translated(mask.rect, offset);
    let x2 = rect.x.saturating_add_unsigned(rect.width);
    let y2 = rect.y.saturating_add_unsigned(rect.height);
    let origins = [
        (rect.x, rect.y),
        (x2, rect.y),
        (x2, y2),
        (rect.x, y2),
    ];
    mask.radii
        .into_iter()
        .zip(origins)
        .enumerate()
        .any(|(corner, (radius, (anchor_x, anchor_y)))| {
            if radius.x == 0 || radius.y == 0 {
                return false;
            }
            let x = if corner == 0 || corner == 3 {
                anchor_x
            } else {
                anchor_x.saturating_sub_unsigned(radius.x)
            };
            let y = if corner <= 1 {
                anchor_y
            } else {
                anchor_y.saturating_sub_unsigned(radius.y)
            };
            intersect(
                bounds,
                Rect {
                    x,
                    y,
                    width: radius.x,
                    height: radius.y,
                },
            )
            .is_some()
        })
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
        TextureMode::Gaussian { texel_step } => {
            words[0] = (texel_step[0] / texture.width() as f32).to_bits();
            words[1] = (texel_step[1] / texture.height() as f32).to_bits();
            (3.0, words)
        }
        TextureMode::MaskGaussian { texel_step } => {
            words[0] = (texel_step[0] / texture.width() as f32).to_bits();
            words[1] = (texel_step[1] / texture.height() as f32).to_bits();
            (4.0, words)
        }
        TextureMode::Shadow {
            mask,
            radii,
            support,
            spread,
            offset,
            inset,
        } => {
            let edges = |rect: Rect| {
                [
                    (rect.x as f32).to_bits(),
                    (rect.y as f32).to_bits(),
                    (rect.x.saturating_add_unsigned(rect.width) as f32).to_bits(),
                    (rect.y.saturating_add_unsigned(rect.height) as f32).to_bits(),
                ]
            };
            words[..4].copy_from_slice(&edges(mask));
            words[4..8].copy_from_slice(&radii.map(|radius| radius.x as f32).map(f32::to_bits));
            words[8..12].copy_from_slice(&radii.map(|radius| radius.y as f32).map(f32::to_bits));
            let reciprocal = if support > 0.0 { 0.5 / support } else { 1024.0 };
            words[12..16].copy_from_slice(&[
                if inset { -reciprocal } else { reciprocal }.to_bits(),
                spread.to_bits(),
                offset[0].to_bits(),
                offset[1].to_bits(),
            ]);
            (5.0, words)
        }
    }
}

fn translated(rectangle: Rect, offset: (i32, i32)) -> Rect {
    Rect {
        x: rectangle.x.saturating_add(offset.0),
        y: rectangle.y.saturating_add(offset.1),
        ..rectangle
    }
}

fn clipped_layer(
    bounds: Rect,
    source: TextureRect,
    clip: Rect,
) -> Option<(Rect, TextureRect)> {
    let clipped = intersect(bounds, clip)?;
    if bounds.width == 0 || bounds.height == 0 {
        return None;
    }
    let scale_x = source.width / bounds.width as f32;
    let scale_y = source.height / bounds.height as f32;
    let offset_x = (clipped.x as f64 - bounds.x as f64) as f32;
    let offset_y = (clipped.y as f64 - bounds.y as f64) as f32;
    Some((
        clipped,
        TextureRect {
            x: source.x + offset_x * scale_x,
            y: source.y + offset_y * scale_y,
            width: clipped.width as f32 * scale_x,
            height: clipped.height as f32 * scale_y,
        },
    ))
}

pub(crate) fn intersect(left: Rect, right: Rect) -> Option<Rect> {
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
    use display_proto::CornerRadius;
    use std::collections::HashSet;

    #[test]
    fn live_context_object_handles_are_globally_unique() {
        // VirGL stores every object kind in one handle table. Reusing a blend
        // handle for a sampler silently replaces the blend; the next retained
        // clear then binds the wrong kind and drops that partial paint.
        let fixed = [
            VERTEX_ELEMENTS,
            VERTEX_SHADER,
            ROUNDED_FRAGMENT_SHADER,
            FLAT_FRAGMENT_SHADER,
            BLEND,
            REPLACE_BLEND,
            DEPTH_STENCIL_ALPHA,
            RASTERIZER,
            SAMPLER_NEAREST_EDGE,
            SAMPLER_LINEAR_EDGE,
            TARGET_SURFACE,
            SOURCE_VIEW,
        ];
        let mut handles = HashSet::new();
        for handle in fixed {
            assert!(
                handles.insert(handle),
                "duplicate VirGL object handle {handle}"
            );
        }
        for repeat in 0..4 {
            for handle in [
                SAMPLER_NEAREST_BACKGROUND + repeat,
                SAMPLER_LINEAR_BACKGROUND + repeat,
            ] {
                assert!(
                    handles.insert(handle),
                    "duplicate VirGL object handle {handle}"
                );
            }
        }
    }

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
    fn coarse_clip_crops_the_quad_and_preserves_texture_mapping() {
        let (bounds, source) = clipped_layer(
            Rect {
                x: 100,
                y: 200,
                width: 400,
                height: 200,
            },
            TextureRect {
                x: 10.0,
                y: 20.0,
                width: 200.0,
                height: 100.0,
            },
            Rect {
                x: 300,
                y: 250,
                width: 100,
                height: 50,
            },
        )
        .unwrap();
        assert_eq!(
            bounds,
            Rect {
                x: 300,
                y: 250,
                width: 100,
                height: 50,
            }
        );
        assert_eq!(
            source,
            TextureRect {
                x: 110.0,
                y: 45.0,
                width: 50.0,
                height: 25.0,
            }
        );
    }

    #[test]
    fn rectangular_masks_stay_in_coarse_clip_and_only_rounded_masks_reach_the_shader() {
        let square = ClipMask {
            rect: Rect {
                x: 5,
                y: 6,
                width: 7,
                height: 8,
            },
            radii: [CornerRadius::default(); 4],
        };
        let rounded = ClipMask {
            rect: Rect {
                x: 50,
                y: 10,
                width: 100,
                height: 20,
            },
            radii: [CornerRadius { x: 10, y: 5 }; 4],
        };
        let corner = Rect {
            x: 53,
            y: 8,
            width: 10,
            height: 5,
        };
        let constants = clip_constants(
            &[square, rounded],
            (3, -2),
            corner,
            Rect::default(),
        );
        assert_eq!(rounded_clip_count(&[square, rounded], (3, -2), corner), 1);
        assert_eq!(
            rounded_clip_count(
                &[rounded],
                (3, -2),
                Rect {
                    x: 70,
                    y: 13,
                    width: 60,
                    height: 10,
                },
            ),
            0
        );
        assert_eq!(f32::from_bits(constants[0]), 63.0);
        assert_eq!(f32::from_bits(constants[1]), 13.0);
        assert_eq!(f32::from_bits(constants[4]), 143.0);
        assert_eq!(f32::from_bits(constants[9]), 23.0);
        assert_eq!(
            [constants[20], constants[21], constants[22], constants[23],].map(f32::from_bits),
            [53.0, 8.0, 153.0, 28.0]
        );
    }
}
