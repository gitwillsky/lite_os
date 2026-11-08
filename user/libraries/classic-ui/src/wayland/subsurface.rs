//! 标准子表面与共享不可变 SHM 图像；协议/像素寿命都属于 Display，不伪造顶层角色。
use super::*;
use linux_uapi::filesystem::ReadOnlyMapping;
use linux_uapi::shared_memory::MemFd;
use wayland_wire::protocol::{
    wl_buffer, wl_region, wl_shm, wl_shm_pool, wl_subcompositor, wl_subsurface,
};

/// 同一 Display 拥有的不可变 SHM 图像身份；可同时供多个子表面使用，不依赖多表面 release 事件。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageId(ObjectId);
pub(super) struct Image {
    mapping: ReadOnlyMapping,
    pool: ObjectId,
    size: (i32, i32),
}
impl Display {
    /// 创建默认同步的子表面。position 仅在父提交时生效，子提交随父原子应用。
    /// 尺寸使用逻辑坐标，输出 scale 来自父的权威配置；非法父/尺寸/协议错误原样返回。
    pub fn create_subsurface(
        &mut self,
        parent: WindowId,
        bounds: canvas::Rect,
    ) -> io::Result<WindowId> {
        let manager = self.subcompositor.ok_or(io::ErrorKind::Unsupported)?;
        let parent_state = self.windows.get(&parent).ok_or(io::ErrorKind::NotFound)?;
        let configuration = Configuration {
            width: bounds.width,
            height: bounds.height,
            scale: parent_state.configuration.scale,
            active: false,
            maximized: false,
            fullscreen: false,
            resizing: false,
        };
        configuration.pixel_size()?;
        let surface = self.transport.allocate(Interface::WlSurface)?;
        let object = self.transport.allocate(Interface::WlSubsurface)?;
        let writer = &mut self.transport.writer;
        wl_compositor::Request::CreateSurface { id: surface }
            .encode(self.compositor, writer)
            .map_err(io::Error::other)?;
        wl_subcompositor::Request::GetSubsurface {
            id: object,
            surface,
            parent: parent.0,
        }
        .encode(manager, writer)
        .map_err(io::Error::other)?;
        wl_subsurface::Request::SetPosition {
            x: bounds.x,
            y: bounds.y,
        }
        .encode(object, writer)
        .map_err(io::Error::other)?;
        let id = WindowId(surface);
        self.windows.insert(
            id,
            Window {
                role: Role::Subsurface { object, parent },
                configuration,
                configured: true,
                mapped: false,
                frame: None,
                history: Default::default(),
                viewport: None,
            },
        );
        Ok(id)
    }
    /// 改变子表面位置/绘图尺寸；下一次父 commit 才发表位置，缓冲规格变化自然作废原绘制历史。
    /// 非子表面、尺寸或协议错误返回 I/O 错误。
    pub fn set_subsurface_bounds(
        &mut self,
        window: WindowId,
        bounds: canvas::Rect,
    ) -> io::Result<()> {
        let (object, parent) = match self.windows.get(&window).map(|window| &window.role) {
            Some(Role::Subsurface { object, parent }) => (*object, *parent),
            _ => return Err(io::ErrorKind::InvalidInput.into()),
        };
        let scale = self
            .windows
            .get(&parent)
            .ok_or(io::ErrorKind::NotFound)?
            .configuration
            .scale;
        let state = self.windows.get_mut(&window).expect("validated subsurface");
        let configuration = Configuration {
            width: bounds.width,
            height: bounds.height,
            scale,
            ..state.configuration
        };
        configuration.pixel_size()?;
        state.configuration = configuration;
        wl_subsurface::Request::SetPosition {
            x: bounds.x,
            y: bounds.y,
        }
        .encode(object, &mut self.transport.writer)
        .map_err(io::Error::other)
    }
    /// 空 region 禁止该表面接收输入，None 恢复默认。变更与下一次 buffer commit 原子生效。
    /// 窗口或协议错误原样返回；不重写应用坐标或合成器命中结果。
    pub fn set_input_region(
        &mut self,
        window: WindowId,
        region: Option<canvas::Rect>,
    ) -> io::Result<()> {
        if !self.windows.contains_key(&window) {
            return Err(io::ErrorKind::NotFound.into());
        }
        let object = region
            .map(|_| self.transport.allocate(Interface::WlRegion))
            .transpose()?;
        let writer = &mut self.transport.writer;
        if let Some((object, region)) = object.zip(region) {
            wl_compositor::Request::CreateRegion { id: object }
                .encode(self.compositor, writer)
                .map_err(io::Error::other)?;
            if !region.is_empty() {
                wl_region::Request::Add {
                    x: region.x,
                    y: region.y,
                    width: region.width,
                    height: region.height,
                }
                .encode(object, writer)
                .map_err(io::Error::other)?;
            }
        }
        wl_surface::Request::SetInputRegion { region: object }
            .encode(window.0, writer)
            .map_err(io::Error::other)?;
        if let Some(object) = object {
            wl_region::Request::Destroy
                .encode(object, writer)
                .map_err(io::Error::other)?;
        }
        Ok(())
    }
    /// 将原始预乘位图一次写入封印 memfd，发布原尺寸 wl_buffer，不在来宾预缩放或建立可写映射。
    /// 图像受 Display 拥有，显式 destroy_image 或连接关闭退休；尺寸、分配、封印及协议错误返回 I/O 错误。
    pub fn create_image(&mut self, bitmap: &crate::Bitmap) -> io::Result<ImageId> {
        let size = (bitmap.width(), bitmap.height());
        let image = bitmap.image();
        if size.0 <= 0
            || size.1 <= 0
            || (size.0 as usize).checked_mul(size.1 as usize) != Some(image.pixels.len())
            || image.pixels.len() > i32::MAX as usize / 4
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let stride = size.0.checked_mul(4).ok_or(io::ErrorKind::InvalidInput)?;
        let memory = MemFd::from_words("wayland-image", image.pixels)?;
        if !linux_uapi::shared_memory::is_immutable(memory.as_fd()) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "shared images require write and size seals",
            ));
        }
        // SAFETY: 上面的内核查询确认写/尺寸封印；封印不可撤销，接收 FD 的另一端也不能写入或截断。
        let mapping = unsafe { ReadOnlyMapping::from_fd(memory.as_fd(), memory.len()) }?;
        let pool = self.create_shm_pool(&memory, memory.len())?;
        let buffer = self.transport.allocate(Interface::WlBuffer)?;
        wl_shm_pool::Request::CreateBuffer {
            id: buffer,
            offset: 0,
            width: size.0,
            height: size.1,
            stride,
            format: wl_shm::Format::Argb8888,
        }
        .encode(pool, &mut self.transport.writer)
        .map_err(io::Error::other)?;
        let id = ImageId(buffer);
        self.images.insert(
            id,
            Image {
                mapping,
                pool,
                size,
            },
        );
        Ok(id)
    }
    /// 标准 null attach + commit；同步子表面在父 commit 时原子撤下，存储仍由 Display/服务器保活。
    /// 未知窗口或协议错误原样返回。
    pub fn unmap_surface(&mut self, window: WindowId) -> io::Result<()> {
        let state = self
            .windows
            .get_mut(&window)
            .ok_or(io::ErrorKind::NotFound)?;
        wl_surface::Request::Attach {
            buffer: None,
            x: 0,
            y: 0,
        }
        .encode(window.0, &mut self.transport.writer)
        .map_err(io::Error::other)?;
        wl_surface::Request::Commit
            .encode(window.0, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        state.mapped = false;
        state.history.invalidate();
        self.transport.flush()
    }

    /// 共享图像的原始像素尺寸；未知或已销毁身份返回 NotFound。
    pub fn image_size(&self, image: ImageId) -> io::Result<(i32, i32)> {
        self.images
            .get(&image)
            .map(|image| image.size)
            .ok_or_else(|| io::ErrorKind::NotFound.into())
    }
    /// 借用同一封印图像的只读像素；用于局部桌面叠加，不保留解码后的第二份像素。
    /// 已退休身份或损坏布局返回 I/O 错误，借用不能跨越图像销毁。
    pub fn image_view(&self, image: ImageId) -> io::Result<canvas::Image<'_>> {
        let image = self.images.get(&image).ok_or(io::ErrorKind::NotFound)?;
        let bytes = image.mapping.as_ref();
        // SAFETY: 页对齐只读映射包含完整初始化的 u32 像素，所有 u32 位型有效；封印禁止并发写/截断。
        let (prefix, pixels, suffix) = unsafe { bytes.align_to::<u32>() };
        if !prefix.is_empty() || !suffix.is_empty() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(canvas::Image {
            pixels,
            width: image.size.0,
            height: image.size.1,
            stride: image.size.0 as usize,
        })
    }
    /// 将子表面排在父或同父兄弟上方；位置与堆叠均随父 commit 原子发表。
    /// 非子表面/非法 sibling 或协议错误返回 I/O 错误。
    pub fn place_subsurface_above(
        &mut self,
        window: WindowId,
        sibling: WindowId,
    ) -> io::Result<()> {
        let (object, parent) = match self.windows.get(&window).map(|state| &state.role) {
            Some(Role::Subsurface { object, parent }) => (*object, *parent),
            _ => return Err(io::ErrorKind::InvalidInput.into()),
        };
        if sibling != parent && !self.windows.get(&sibling).is_some_and(|state|
            matches!(state.role, Role::Subsurface { parent: sibling_parent, .. } if sibling_parent == parent) && sibling != window) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        wl_subsurface::Request::PlaceAbove { sibling: sibling.0 }
            .encode(object, &mut self.transport.writer)
            .map_err(io::Error::other)
    }
    /// 子表面共享不可变图像，按逻辑尺寸作 viewport 最近邻采样；不请求每瓦片 callback。
    /// 父提交负责原子呈现。未知身份、尺寸或协议错误原样返回。
    pub fn present_image(&mut self, window: WindowId, image: ImageId) -> io::Result<()> {
        let size = self.image_size(image)?;
        let state = self.windows.get(&window).ok_or(io::ErrorKind::NotFound)?;
        let destination = (state.configuration.width, state.configuration.height);
        self.set_viewport_destination(window, (destination != size).then_some(destination))?;
        let writer = &mut self.transport.writer;
        wl_surface::Request::SetBufferScale { scale: 1 }
            .encode(window.0, writer)
            .map_err(io::Error::other)?;
        wl_surface::Request::Attach {
            buffer: Some(image.0),
            x: 0,
            y: 0,
        }
        .encode(window.0, writer)
        .map_err(io::Error::other)?;
        wl_surface::Request::DamageBuffer {
            x: 0,
            y: 0,
            width: size.0,
            height: size.1,
        }
        .encode(window.0, writer)
        .map_err(io::Error::other)?;
        wl_surface::Request::Commit
            .encode(window.0, writer)
            .map_err(io::Error::other)?;
        self.transport.flush()?;
        self.windows
            .get_mut(&window)
            .expect("validated image surface")
            .mapped = true;
        Ok(())
    }
    /// 退休图像对象，不重用封印存储；服务器/在途 surface 的真实引用负责其余使用期。
    /// 已销毁身份或协议错误原样返回。
    pub fn destroy_image(&mut self, image: ImageId) -> io::Result<()> {
        let image_state = self.images.remove(&image).ok_or(io::ErrorKind::NotFound)?;
        wl_buffer::Request::Destroy
            .encode(image.0, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        wl_shm_pool::Request::Destroy
            .encode(image_state.pool, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        self.transport.flush()
    }
}
