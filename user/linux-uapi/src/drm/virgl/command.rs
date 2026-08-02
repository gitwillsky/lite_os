//! Typed encoder for the stable VirGL Gallium command stream.

const CREATE_OBJECT: u32 = 1;
const BIND_OBJECT: u32 = 2;
const DESTROY_OBJECT: u32 = 3;
const SET_VIEWPORT: u32 = 4;
const SET_FRAMEBUFFER: u32 = 5;
const SET_VERTEX_BUFFERS: u32 = 6;
const CLEAR: u32 = 7;
const DRAW_VBO: u32 = 8;
const SET_SAMPLER_VIEWS: u32 = 10;
const SET_CONSTANT_BUFFER: u32 = 12;
const BIND_SAMPLER_STATES: u32 = 18;
const BIND_SHADER: u32 = 31;
const LINK_SHADER: u32 = 52;

/// VirGL context object type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ObjectKind {
    Blend = 1,
    Rasterizer = 2,
    DepthStencilAlpha = 3,
    Shader = 4,
    VertexElements = 5,
    SamplerView = 6,
    SamplerState = 7,
    Surface = 8,
}

/// Gallium shader stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ShaderStage {
    Vertex = 0,
    Fragment = 1,
}

/// Texture minification, magnification and mip filter selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SamplerFilter {
    Nearest,
    Linear,
}

/// Gallium texture coordinate behavior for one sampler axis.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum SamplerWrap {
    Repeat = 0,
    ClampToEdge = 2,
    ClampToBorder = 3,
}

/// Append-only encoder whose output is accepted by `DRM_IOCTL_VIRTGPU_EXECBUFFER`.
pub struct CommandEncoder {
    words: Vec<u32>,
}

impl CommandEncoder {
    /// Creates an empty command stream.
    pub fn new() -> Self {
        Self { words: Vec::new() }
    }

    /// Returns the encoded native-endian dwords.
    pub fn words(&self) -> &[u32] {
        &self.words
    }

    /// Returns the encoded dword count for bounded execbuffer batching.
    pub fn len(&self) -> usize {
        self.words.len()
    }

    /// @description 判断编码器是否尚未包含命令字。
    /// @return 没有任何已编码命令时返回 `true`。
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Creates one render-target surface object.
    pub fn create_surface(&mut self, handle: u32, resource: u32, format: u32) {
        self.header(CREATE_OBJECT, ObjectKind::Surface, 5);
        self.words.extend([handle, resource, format, 0, 0]);
    }

    /// Creates the position/texture-coordinate vertex declaration.
    pub fn create_textured_vertex_elements(&mut self, handle: u32) {
        self.header(CREATE_OBJECT, ObjectKind::VertexElements, 9);
        self.words.extend([handle, 0, 0, 0, 29, 8, 0, 0, 29]);
    }

    /// Creates a TGSI-text shader object.
    pub fn create_shader(&mut self, handle: u32, stage: ShaderStage, source: &str) {
        let mut bytes = source.as_bytes().to_vec();
        bytes.push(0);
        let string_words = bytes.len().div_ceil(4);
        // VirGLRenderer uses this field as the TGSI parser's token capacity. One
        // source byte per token is a strict upper bound for TGSI text and keeps
        // the allocation proportional to the submitted shader. A fixed value
        // rejects larger generated CSS shaders after SUBMIT_3D has already been
        // acknowledged, leaving the scanout silently black.
        let token_capacity =
            u32::try_from(source.len()).expect("VirGL shader source length exceeds protocol u32");
        self.header_raw(
            CREATE_OBJECT,
            ObjectKind::Shader as u32,
            5 + string_words as u32,
        );
        self.words
            .extend([handle, stage as u32, bytes.len() as u32, token_capacity, 0]);
        for chunk in bytes.chunks(4) {
            let mut word = [0; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            self.words.push(u32::from_ne_bytes(word));
        }
    }

    /// Creates premultiplied-alpha OVER blending on color target zero.
    pub fn create_premultiplied_blend(&mut self, handle: u32) {
        self.header(CREATE_OBJECT, ObjectKind::Blend, 11);
        self.words.extend([handle, 0, 0]);
        let enabled = 1 | (1 << 4) | (0x13 << 9) | (1 << 17) | (0x13 << 22) | (0xf << 27);
        self.words.push(enabled);
        self.words.extend([0xf << 27; 7]);
    }

    /// Creates disabled depth/stencil/alpha testing.
    pub fn create_depth_stencil_alpha(&mut self, handle: u32) {
        self.header(CREATE_OBJECT, ObjectKind::DepthStencilAlpha, 5);
        self.words.extend([handle, 0, 0, 0, 0]);
    }

    /// Creates a triangle rasterizer matching pixel-center rules.
    pub fn create_rasterizer(&mut self, handle: u32) {
        self.header(CREATE_OBJECT, ObjectKind::Rasterizer, 9);
        let state = (1 << 1) | (1 << 29) | (1 << 30);
        self.words.extend([
            handle,
            state,
            1.0f32.to_bits(),
            0,
            0,
            1.0f32.to_bits(),
            0,
            0,
            0,
        ]);
    }

    /// Creates one sampler with explicit filtering and independent axis wrapping.
    pub fn create_sampler_state(
        &mut self,
        handle: u32,
        filter: SamplerFilter,
        wrap_s: SamplerWrap,
        wrap_t: SamplerWrap,
    ) {
        self.header(CREATE_OBJECT, ObjectKind::SamplerState, 9);
        let wraps =
            wrap_s as u32 | ((wrap_t as u32) << 3) | ((SamplerWrap::ClampToEdge as u32) << 6);
        let filters = if filter == SamplerFilter::Linear {
            (1 << 9) | (1 << 11) | (1 << 13)
        } else {
            0
        };
        self.words
            .extend([handle, wraps | filters, 0, 0, 0, 0, 0, 0, 0]);
    }

    /// Creates one 2D sampler view with identity channel swizzle.
    pub fn create_sampler_view(&mut self, handle: u32, resource: u32, format: u32) {
        self.header(CREATE_OBJECT, ObjectKind::SamplerView, 6);
        let swizzle = (1 << 3) | (2 << 6) | (3 << 9);
        // The sampler-view format word also owns the Gallium texture target.
        // Omitting it declares a buffer view, so 2D render targets later sample
        // only a host-dependent slice instead of the rendered surface.
        let view_format = format | (super::PIPE_TEXTURE_2D << 24);
        self.words
            .extend([handle, resource, view_format, 0, 0, swizzle]);
    }

    /// Binds one context object.
    pub fn bind_object(&mut self, kind: ObjectKind, handle: u32) {
        self.header(BIND_OBJECT, kind, 1);
        self.words.push(handle);
    }

    /// Destroys one context object after its final use in this stream.
    pub fn destroy_object(&mut self, kind: ObjectKind, handle: u32) {
        self.header(DESTROY_OBJECT, kind, 1);
        self.words.push(handle);
    }

    /// Binds one shader stage.
    pub fn bind_shader(&mut self, stage: ShaderStage, handle: u32) {
        self.header_raw(BIND_SHADER, 0, 2);
        self.words.extend([handle, stage as u32]);
    }

    /// Links the current vertex and fragment programs.
    pub fn link_shaders(&mut self, vertex: u32, fragment: u32) {
        self.header_raw(LINK_SHADER, 0, 6);
        self.words.extend([vertex, fragment, 0, 0, 0, 0]);
    }

    /// Selects one color surface as the framebuffer.
    pub fn set_framebuffer(&mut self, surface: u32) {
        self.header_raw(SET_FRAMEBUFFER, 0, 3);
        self.words.extend([1, 0, surface]);
    }

    /// Sets the full Gallium viewport with Y inversion for a Y0-top render target.
    pub fn set_viewport(&mut self, width: u32, height: u32) {
        let half_width = width as f32 / 2.0;
        let half_height = height as f32 / 2.0;
        self.header_raw(SET_VIEWPORT, 0, 7);
        self.words.extend([
            0,
            half_width.to_bits(),
            (-half_height).to_bits(),
            0.5f32.to_bits(),
            half_width.to_bits(),
            half_height.to_bits(),
            0.5f32.to_bits(),
        ]);
    }

    /// Selects a single interleaved position/UV vertex buffer.
    pub fn set_vertex_buffer(&mut self, resource: u32) {
        self.header_raw(SET_VERTEX_BUFFERS, 0, 3);
        self.words.extend([16, 0, resource]);
    }

    /// Binds one texture and sampler to fragment slot zero.
    pub fn set_texture(&mut self, view: u32, sampler: u32) {
        self.header_raw(SET_SAMPLER_VIEWS, 0, 3);
        self.words.extend([ShaderStage::Fragment as u32, 0, view]);
        self.header_raw(BIND_SAMPLER_STATES, 0, 3);
        self.words
            .extend([ShaderStage::Fragment as u32, 0, sampler]);
    }

    /// Replaces one shader constant buffer with inline dwords.
    pub fn set_constants(&mut self, stage: ShaderStage, words: &[u32]) {
        self.header_raw(SET_CONSTANT_BUFFER, 0, 2 + words.len() as u32);
        self.words.extend([stage as u32, 0]);
        self.words.extend_from_slice(words);
    }

    /// Clears color target zero.
    pub fn clear(&mut self, color: [f32; 4]) {
        self.header_raw(CLEAR, 0, 8);
        self.words.extend([
            1 << 2,
            color[0].to_bits(),
            color[1].to_bits(),
            color[2].to_bits(),
            color[3].to_bits(),
            0,
            0,
            0,
        ]);
    }

    /// Draws non-indexed triangles from the active vertex buffer.
    pub fn draw_triangles(&mut self, start: u32, count: u32) {
        self.header_raw(DRAW_VBO, 0, 12);
        self.words.extend([
            start,
            count,
            4,
            0,
            1,
            0,
            0,
            0,
            0,
            0,
            count.saturating_sub(1),
            0,
        ]);
    }

    fn header(&mut self, command: u32, object: ObjectKind, length: u32) {
        self.header_raw(command, object as u32, length);
    }

    fn header_raw(&mut self, command: u32, object: u32, length: u32) {
        self.words.push(command | (object << 8) | (length << 16));
    }
}

impl Default for CommandEncoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_packet_matches_virgl_protocol_layout() {
        let mut encoder = CommandEncoder::new();
        encoder.create_surface(11, 29, 2);
        assert_eq!(
            encoder.words(),
            &[1 | (8 << 8) | (5 << 16), 11, 29, 2, 0, 0]
        );
    }

    #[test]
    fn shader_packet_is_nul_terminated_and_dword_aligned() {
        let mut encoder = CommandEncoder::new();
        encoder.create_shader(4, ShaderStage::Fragment, "END\n");
        assert_eq!(encoder.words()[0] >> 16, 7);
        assert_eq!(encoder.words()[1..6], [4, 1, 5, 4, 0]);
        assert_eq!(encoder.words()[6].to_ne_bytes(), *b"END\n");
        assert_eq!(encoder.words()[7].to_ne_bytes(), [0, 0, 0, 0]);
    }

    #[test]
    fn shader_token_capacity_scales_with_generated_source() {
        let source = "0: MOV OUT[0], IN[0]\n".repeat(400);
        let mut encoder = CommandEncoder::new();
        encoder.create_shader(9, ShaderStage::Fragment, &source);

        assert_eq!(encoder.words()[4], source.len() as u32);
        assert!(encoder.words()[4] > 300);
    }

    #[test]
    fn shader_link_packet_uses_stable_virgl_command_number() {
        let mut encoder = CommandEncoder::new();
        encoder.link_shaders(2, 3);

        assert_eq!(encoder.words(), &[52 | (6 << 16), 2, 3, 0, 0, 0, 0]);
    }

    #[test]
    fn sampler_view_declares_a_2d_texture_target() {
        let mut encoder = CommandEncoder::new();
        encoder.create_sampler_view(19, 7, 1);

        assert_eq!(
            encoder.words(),
            &[1 | (6 << 8) | (6 << 16), 19, 7, 1 | (2 << 24), 0, 0, 1672]
        );
    }

    #[test]
    fn y0_top_viewport_inverts_gallium_y_scale() {
        let mut encoder = CommandEncoder::new();
        encoder.set_viewport(3008, 1692);

        assert_eq!(f32::from_bits(encoder.words()[3]), -846.0);
        assert_eq!(f32::from_bits(encoder.words()[6]), 846.0);
    }
}
