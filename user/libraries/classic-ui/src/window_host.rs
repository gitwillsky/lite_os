//! 窗口级共同 owner：装饰、绘图存储与未提交失效，协议状态仍只属于 Display。
use crate::{
    Configuration, Decoration, DecorationAction, Display, Event, EventKind, FrameKind, Painter,
    Resources, WindowId,
};
use canvas::{Rect, Surface};
use classic_theme::SystemColor;
use std::io;

/// 默认事件处理的结果；关闭由应用决定，绘制状态由 WindowHost 保留。
pub struct HostEvent {
    pub close: bool,
}
/// 一个窗口的绘制/装饰 owner；不复制 configure、在途 callback 或 buffer.release 状态。
pub struct WindowHost {
    window: WindowId,
    decoration: Decoration,
    pixels: Vec<u32>,
    dirty: bool,
    resource_generation: u64,
}
impl WindowHost {
    /// 绑定 Display 已创建的窗口；第一次 configure 后完成首次绘制。
    pub fn new(window: WindowId, kind: FrameKind) -> Self {
        Self {
            window,
            decoration: Decoration::new(kind),
            pixels: Vec::new(),
            dirty: true,
            resource_generation: 0,
        }
    }
    /// 返回协议窗口标识；仅可交给创建它的 Display。
    pub fn window(&self) -> WindowId {
        self.window
    }
    /// 使用共享资源上下文的程序图标，避免每个窗口重新读取同一素材。
    pub fn set_icon(&mut self, icon: Option<std::rc::Rc<crate::Bitmap>>) {
        self.decoration.set_icon(icon);
        self.invalidate();
    }

    /// 保留重绘请求，直到成功提交；等待 callback 或 release 时不会丢失。
    pub fn invalidate(&mut self) {
        self.dirty = true;
    }
    /// 当前配置下能否使用状态栏尺寸手柄；没有 configure 时为 false。
    pub fn size_grip_allowed(&self, display: &Display) -> bool {
        display
            .configuration(self.window)
            .is_some_and(|configuration| self.decoration.size_grip_allowed(configuration))
    }

    /// 登记客户区内容的尺寸手柄矩形（surface 局部坐标），交给装饰统一处理缩放命中。
    pub fn set_size_grip(&mut self, grip: Option<Rect>) {
        self.decoration.set_size_grip(grip);
    }

    /// 返回当前配置下的客户区；没有 configure 时返回 None。
    pub fn client_rect(&self, display: &Display) -> Option<Rect> {
        display
            .configuration(self.window)
            .map(|configuration| self.decoration.client_rect(configuration))
    }
    /// 固定对话框保持声明宽度，按该宽度下的内容高度扩展；外框/标题栏由当前装饰几何补齐。
    /// 可缩放窗口保持用户尺寸，过长的单行标签继续按控件契约截断。
    /// 返回是否扩大并失效；Display 的协议或尺寸错误原样返回。
    pub fn fit_content(&mut self, display: &mut Display, height: i32) -> io::Result<bool> {
        if !self.decoration.fixed() {
            return Ok(false);
        }
        let Some(configuration) = display.configuration(self.window) else {
            return Ok(false);
        };
        let client = self.decoration.client_rect(configuration);
        let outer = (
            configuration.width,
            configuration.height + (height - client.height).max(0),
        );
        let grown = display.grow_fixed_size(self.window, outer)?;
        self.dirty |= grown;
        Ok(grown)
    }

    /// 处理标准装饰与失效；不属于本窗口的事件被忽略，协议失败返回错误。
    pub fn event(&mut self, display: &mut Display, event: &Event) -> io::Result<HostEvent> {
        if event.window != self.window {
            return Ok(HostEvent { close: false });
        }
        let mut close = matches!(event.kind, EventKind::Close);
        if let Some(configuration) = display.configuration(self.window) {
            let before = self.decoration.pressed_button(configuration);
            match self.decoration.event(&event.kind, configuration) {
                Some(DecorationAction::Request(request)) => {
                    display.request(self.window, request)?
                }
                Some(DecorationAction::Cursor(shape)) => display.set_cursor_shape(shape)?,
                Some(DecorationAction::Close) => close = true,
                None => {}
            }
            self.dirty |= before != self.decoration.pressed_button(configuration);
        }
        // 窗口 owner 只为配置/焦点/装饰变化失效；客户区控件根据自身状态变化申请绘制。
        // 普通指针移动和按键释放不改变像素，不能因此触发整窗光栅、SHM 复制和合成。
        self.dirty |= matches!(event.kind, EventKind::Configure(_) | EventKind::Focus(_));
        Ok(HostEvent { close })
    }
    /// 在需要且允许提交时绘制完整窗口；draw 只负责客户区，尺寸为逻辑像素。
    ///
    /// 返回 true 表示完成提交，false 表示无失效或等待配置/callback/缓冲。
    /// 字体、绘制与 I/O 错误保留失效并返回 Err；不执行应用事件回调。
    pub fn paint(
        &mut self,
        display: &mut Display,
        resources: &mut Resources,
        title: &str,
        draw: impl FnOnce(&mut Painter<'_>, Rect, Configuration, &Display) -> io::Result<()>,
    ) -> io::Result<bool> {
        self.dirty |= self.resource_generation != resources.generation();
        if !self.dirty || !display.ready_to_draw(self.window) {
            return Ok(false);
        }
        let Some(configuration) = display.configuration(self.window) else {
            return Ok(false);
        };
        let (width, height) = configuration.pixel_size()?;
        let font = resources.fonts(configuration.scale)?;
        self.pixels.resize(width as usize * height as usize, 0);
        let surface = Surface::new(&mut self.pixels, width, height, width as usize)
            .ok_or_else(|| io::Error::other("invalid window drawing surface"))?;
        let mut painter = Painter::new(surface, resources.theme(), &font, configuration.scale);
        let client = self.decoration.paint(&mut painter, configuration, title);
        painter.fill(client, SystemColor::ButtonFace);
        let mut result = Ok(());
        painter.clipped(client, |painter| {
            result = draw(painter, client, configuration, display)
        });
        result?;
        match display.present(self.window, &self.pixels) {
            Ok(()) => {
                self.dirty = false;
                self.resource_generation = resources.generation();
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(error) => Err(error),
        }
    }
    /// 释放窗口协议角色；尚未 release 的内存仍由 Display 保活。
    pub fn close(self, display: &mut Display) -> io::Result<()> {
        display.destroy_window(self.window)
    }
}
