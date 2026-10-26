//! 标准 VirGL2/DRM 合成：纹理由已提交 Content 持有，帧只借用并保活资源到 page-flip 完成。
use super::Drawable;
use canvas::{Rect, Surface};
use classic_theme::{SystemColor, Theme};
use linux_uapi::drm::{
    CommandEncoder, DrmDevice, ObjectKind, SamplerFilter, SamplerWrap, ShaderStage, VirglContext,
    VirglResource,
};
use std::{io, rc::Rc};
const VERTEX: u32 = 1;
const FRAGMENT: u32 = 2;
const ELEMENTS: u32 = 3;
const OVER: u32 = 4;
const DEPTH: u32 = 6;
const RASTER: u32 = 7;
const SAMPLER: u32 = 8;
const VIEW: u32 = 9;
const TARGET: u32 = 10;
const VERTEX_SOURCE: &str = "VERT\nDCL IN[0]\nDCL IN[1]\nDCL OUT[0], POSITION\nDCL OUT[1], GENERIC[0]\nMOV OUT[0], IN[0]\nMOV OUT[1], IN[1]\nEND\n";
const FRAGMENT_SOURCE: &str = "FRAG\nDCL IN[0], GENERIC[0], LINEAR\nDCL OUT[0], COLOR\nDCL SAMP[0]\nDCL SVIEW[0], 2D, FLOAT\nTEX OUT[0], IN[0], SAMP[0], 2D\nEND\n";
struct Target {
    resource: VirglResource,
    framebuffer: u32,
    vertices: Option<VirglResource>,
    device: DrmDevice,
}
impl Drop for Target {
    fn drop(&mut self) {
        // FBO 先于 GEM 释放，构造后的任一失败路径也经过此处。
        let _ = self.device.remove_framebuffer(self.framebuffer);
    }
}
struct Quad {
    texture: Rc<VirglResource>,
    rect: Rect,
    source: canvas::ImageSource,
}
/// 一条 DRM OFD 上的 GPU 合成资源；不接收应用协议，不保存第二份 surface 内容表。
pub(crate) struct Gpu {
    context: VirglContext,
    targets: [Target; 2],
    // OWNER: 当前一帧独占引用集合。只有 DRM flip 完成后清除，Content 提前销毁也不会释放在途纹理。
    // FAILURE: 没有这些引用时，客户端重绘/退出可能在 host 取样前关闭 GEM；不能靠对象 ID 缓存保活。
    retained: Vec<Rc<VirglResource>>,
}
impl Gpu {
    pub(crate) fn new(device: &DrmDevice, width: u32, height: u32) -> io::Result<Self> {
        let context = device.initialize_virgl("liteos-compositor")?;
        let target = || -> io::Result<Target> {
            let resource = context.create_render_target(width, height)?;
            let framebuffer = device.add_virgl_framebuffer(&resource, 24)?;
            Ok(Target {
                resource,
                framebuffer,
                vertices: None,
                device: device.clone(),
            })
        };
        let targets = [target()?, target()?];
        let mut commands = CommandEncoder::new();
        commands.create_shader(VERTEX, ShaderStage::Vertex, VERTEX_SOURCE);
        commands.create_shader(FRAGMENT, ShaderStage::Fragment, FRAGMENT_SOURCE);
        commands.link_shaders(VERTEX, FRAGMENT);
        commands.bind_shader(ShaderStage::Vertex, VERTEX);
        commands.bind_shader(ShaderStage::Fragment, FRAGMENT);
        commands.create_textured_vertex_elements(ELEMENTS);
        commands.bind_object(ObjectKind::VertexElements, ELEMENTS);
        commands.create_premultiplied_blend(OVER);
        commands.bind_object(ObjectKind::Blend, OVER);
        commands.create_depth_stencil_alpha(DEPTH);
        commands.bind_object(ObjectKind::DepthStencilAlpha, DEPTH);
        commands.create_rasterizer(RASTER);
        commands.bind_object(ObjectKind::Rasterizer, RASTER);
        commands.create_sampler_state(
            SAMPLER,
            SamplerFilter::Nearest,
            SamplerWrap::ClampToEdge,
            SamplerWrap::ClampToEdge,
        );
        context.exec(commands.words(), &[])?;
        Ok(Self {
            context,
            targets,
            retained: Vec::new(),
        })
    }
    pub(crate) fn framebuffer(&self, back: usize) -> u32 {
        self.targets[back].framebuffer
    }
    pub(crate) fn complete(&mut self) {
        self.retained.clear();
    }
    /// 直接把启动画面写入 GPU 扫描目标的 guest backing，并用标准 transfer 发布。
    /// 调用方保证该 back 没有 flip 在途；不创建第二对 CPU framebuffer。
    pub(crate) fn paint_boot(
        &mut self,
        back: usize,
        scene: &[u8],
        permille: u16,
    ) -> io::Result<()> {
        let target = &mut self.targets[back].resource;
        crate::boot::paint(target, scene, permille).map_err(io::Error::other)?;
        target.transfer_to_host(0, 0, target.width(), target.height())
    }
    /// 合成一帧，保活纹理到 complete；调用方保证没有上一帧 flip 在途。
    /// 主机声明支持 VirGL2 后发生的错误直接返回，不悄悄切换到另一条显示路径。
    pub(crate) fn render(
        &mut self,
        back: usize,
        theme: &Theme,
        windows: &[Drawable<'_>],
        has_overlay: bool,
        overlay: impl FnOnce(Surface<'_>),
    ) -> io::Result<()> {
        let (width, height) = (
            self.targets[back].resource.width(),
            self.targets[back].resource.height(),
        );
        let mut quads = Vec::with_capacity(windows.len() + usize::from(has_overlay));
        let screen = Rect::new(0, 0, width as i32, height as i32);
        for window in windows {
            let rect = window.rect;
            if rect.intersect(screen).is_empty() {
                continue;
            }
            quads.push(Quad {
                texture: window.content.texture(&self.context)?,
                rect,
                source: window.content.source,
            });
        }
        if has_overlay {
            // 合成器自身的菜单/切换面板也作为透明纹理取样，不退回整屏 CPU 合成。
            let mut pixels = vec![0; width as usize * height as usize];
            let surface = Surface::new(&mut pixels, width as i32, height as i32, width as usize)
                .expect("overlay geometry");
            overlay(surface);
            let content = crate::wayland::Content::new(width as usize, height as usize, pixels);
            quads.push(Quad {
                texture: content.texture(&self.context)?,
                rect: screen,
                source: content.source,
            });
        }
        let bytes = quads
            .len()
            .checked_mul(6 * 16)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| io::Error::other("composition vertex buffer is too large"))?
            .max(16);
        let target = &mut self.targets[back];
        if target
            .vertices
            .as_ref()
            .is_none_or(|vertices| vertices.size() < bytes as usize)
        {
            target.vertices = Some(self.context.create_vertex_buffer(bytes)?);
        }
        let vertices = target.vertices.as_mut().expect("vertex buffer prepared");
        let mut offset = 0;
        for quad in &quads {
            let visible = quad.rect.intersect(screen);
            let vertex = |x: i32, y: i32| {
                let u = f64::from(x - quad.rect.x) / f64::from(quad.rect.width);
                let v = f64::from(y - quad.rect.y) / f64::from(quad.rect.height);
                let (u, v) = quad.source.point(u, v);
                [
                    2.0 * x as f32 / width as f32 - 1.0,
                    1.0 - 2.0 * y as f32 / height as f32,
                    (u / f64::from(quad.texture.width())) as f32,
                    (v / f64::from(quad.texture.height())) as f32,
                ]
            };
            for (x, y) in [
                (visible.x, visible.y),
                (visible.right(), visible.y),
                (visible.right(), visible.bottom()),
                (visible.x, visible.y),
                (visible.right(), visible.bottom()),
                (visible.x, visible.bottom()),
            ] {
                for value in vertex(x, y) {
                    vertices.bytes_mut()[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
                    offset += 4;
                }
            }
        }
        vertices.transfer_buffer_to_host()?;
        let mut commands = CommandEncoder::new();
        commands.create_surface(
            TARGET,
            target.resource.resource_id(),
            target.resource.format(),
        );
        commands.set_framebuffer(TARGET);
        commands.set_viewport(width, height);
        commands.set_vertex_buffer(vertices.resource_id());
        let background = theme.color(SystemColor::Background);
        commands.clear([
            f32::from(background.red) / 255.,
            f32::from(background.green) / 255.,
            f32::from(background.blue) / 255.,
            1.,
        ]);
        for (index, quad) in quads.iter().enumerate() {
            commands.create_sampler_view(VIEW, quad.texture.resource_id(), quad.texture.format());
            commands.set_texture(VIEW, SAMPLER);
            commands.draw_triangles(index as u32 * 6, 6);
            commands.destroy_object(ObjectKind::SamplerView, VIEW);
        }
        commands.destroy_object(ObjectKind::Surface, TARGET);
        let mut seen = std::collections::BTreeSet::new();
        let mut resources = vec![&target.resource, &*vertices];
        for quad in &quads {
            if seen.insert(quad.texture.resource_id()) {
                resources.push(quad.texture.as_ref());
            }
        }
        self.context.exec(commands.words(), &resources)?;
        self.retained = quads.into_iter().map(|quad| quad.texture).collect();
        Ok(())
    }
}
