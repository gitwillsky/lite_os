//! 窗口对象与 z-order 栈。
//!
//! 窗口上限 8，固定数组 + 空闲槽管理（无堆分配）。`order` 为 z-order 栈
//! （底→顶，顶在尾），`focused` 记录键盘焦点窗口的槽位。
//!
//! 窗口内容像素来自客户端的 dumb buffer：`CREATE_SURFACE` 提及时 handle
//! 所有权转移给桌面，桌面 `MAP_DUMB` + `mmap` 后只读合成；窗口销毁时由桌面
//! `munmap` + `DESTROY_DUMB`（客户端绝不销毁 handle）。内核 dumb pitch 恒为
//! `width * 4`，故映射大小为 `width * 4 * height`。
//!
//! 窗口状态机：`Normal` / `Minimized` / `Maximized`。最小化窗口不参与合成与
//! hit-test，还原时回到最小化前的状态；最大化时外框固定为 work area（内容尺寸
//! 不变，仍锚定左上角），并记录还原所需的外框原点。

use crate::{
    chrome::{self, Button, Layout},
    ffi,
    scanout::{self, Rect},
};

/// 同时存在的窗口上限。
pub const MAX_WINDOWS: usize = 8;
/// 标题字节上限（超出截断）。
pub const MAX_TITLE: usize = 64;
/// 缩放命中带宽度（px，1× 基准 4）：右 / 下边缘与右下角的触发区（不含标题栏）。
pub const RESIZE_BAND: i32 = 4 * chrome::SCALE;

/// hit-test 结果：指针落在窗口的哪个区域。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// 关闭按钮。
    CloseButton,
    /// 最大化 / 还原按钮。
    MaximizeButton,
    /// 最小化按钮。
    MinimizeButton,
    /// 标题栏（含边框，可拖动）。
    TitleBar,
    /// 内容区。
    Content,
    /// 右边缘缩放命中带。
    ResizeEast,
    /// 下边缘缩放命中带。
    ResizeSouth,
    /// 右下角缩放命中带。
    ResizeSouthEast,
}

impl Region {
    /// 标题栏按钮区域映射为 [`Button`]；非按钮区域返回 `None`。
    pub fn button(self) -> Option<Button> {
        match self {
            Region::CloseButton => Some(Button::Close),
            Region::MaximizeButton => Some(Button::Maximize),
            Region::MinimizeButton => Some(Button::Minimize),
            _ => None,
        }
    }

    /// 是否为缩放命中带（及方向：东 / 南）。
    pub fn resize_edges(self) -> Option<(bool, bool)> {
        match self {
            Region::ResizeEast => Some((true, false)),
            Region::ResizeSouth => Some((false, true)),
            Region::ResizeSouthEast => Some((true, true)),
            _ => None,
        }
    }
}

/// 窗口状态（最小化时 `restore` 字段记住还原目标）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// 普通浮动窗口。
    Normal,
    /// 最小化：不参与合成与 hit-test。
    Minimized,
    /// 最大化：外框固定为 work area，禁止移动与缩放拖动。
    Maximized,
}

pub struct Window {
    /// 桌面分配的 surface id（协议内唯一，不含 0）。
    pub surface_id: u32,
    /// 拥有者 client 在 `Clients` 中的索引。
    pub client: usize,
    /// 客户端创建、所有权已转移给桌面的 GEM handle。
    pub gem_handle: u32,
    pixels: *mut u32,
    map_size: usize,
    content_width: usize,
    content_height: usize,
    /// 外框原点的屏幕 x 坐标。
    pub x: i32,
    /// 外框原点的屏幕 y 坐标。
    pub y: i32,
    /// 是否带 SSD 装饰。
    pub decorated: bool,
    title: [u8; MAX_TITLE],
    title_len: usize,
    state: State,
    /// 最小化前的状态（`state == Minimized` 时有效）。
    restore_state: State,
    /// 最大化前的外框原点（`state == Maximized` 时有效）。
    saved_origin: (i32, i32),
    /// 最大化前的内容尺寸（还原时 `CONFIGURE` 的建议尺寸）。
    saved_content: (usize, usize),
    /// 最大化时的外框矩形（work area；`state == Maximized` 时有效）。
    max_rect: Rect,
    /// 还原后等待客户端 `SET_BUFFER` 期间保持的视觉外框（还原前的 work
    /// area 外框）；`SET_BUFFER` 到达即清除，外框按新内容尺寸自适应。
    pending_outer: Option<Rect>,
    alive: bool,
}

impl Window {
    const EMPTY: Self = Self {
        surface_id: 0,
        client: 0,
        gem_handle: 0,
        pixels: core::ptr::null_mut(),
        map_size: 0,
        content_width: 0,
        content_height: 0,
        x: 0,
        y: 0,
        decorated: false,
        title: [0; MAX_TITLE],
        title_len: 0,
        state: State::Normal,
        restore_state: State::Normal,
        saved_origin: (0, 0),
        saved_content: (0, 0),
        max_rect: Rect::new(0, 0, 0, 0),
        pending_outer: None,
        alive: false,
    };

    /// 外框尺寸（含装饰；最大化 / 还原过渡期为保持的视觉尺寸）。
    fn outer_size(&self) -> (i32, i32) {
        if let Some(rect) = self.pending_outer {
            return (rect.width(), rect.height());
        }
        if self.state == State::Maximized {
            return (self.max_rect.width(), self.max_rect.height());
        }
        chrome::outer_size(
            self.content_width as i32,
            self.content_height as i32,
            self.decorated,
        )
    }

    /// 装饰布局（相对外框原点）。
    pub fn layout(&self) -> Layout {
        let (width, height) = self.outer_size();
        chrome::layout(width, height, self.decorated)
    }

    /// 外框的屏幕矩形。
    pub fn outer_rect(&self) -> Rect {
        if let Some(rect) = self.pending_outer {
            return rect;
        }
        if self.state == State::Maximized {
            return self.max_rect;
        }
        let (width, height) = self.outer_size();
        Rect::new(self.x, self.y, self.x + width, self.y + height)
    }

    /// 内容区的屏幕矩形（最大化 / 还原过渡期内容尺寸不变，锚定视觉外框
    /// 左上角）。
    pub fn content_rect(&self) -> Rect {
        let outer = self.outer_rect();
        let layout = self.layout();
        let (ox, oy) = layout.content_origin;
        Rect::new(
            outer.x1 + ox,
            outer.y1 + oy,
            outer.x1 + ox + self.content_width as i32,
            outer.y1 + oy + self.content_height as i32,
        )
    }

    /// 某个标题栏按钮的屏幕矩形；`region` 非按钮时返回空矩形。
    pub fn button_rect(&self, region: Region) -> Rect {
        let outer = self.outer_rect();
        let layout = self.layout();
        let local = match region {
            Region::CloseButton => layout.close_button,
            Region::MaximizeButton => layout.maximize_button,
            Region::MinimizeButton => layout.minimize_button,
            _ => return Rect::new(0, 0, 0, 0),
        };
        Rect::new(
            local.x1 + outer.x1,
            local.y1 + outer.y1,
            local.x2 + outer.x1,
            local.y2 + outer.y1,
        )
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// 几何是否被锁定（最大化或还原过渡期）：锁定期间禁止移动与缩放拖动，
    /// hit-test 不提供缩放命中带。
    pub fn geometry_locked(&self) -> bool {
        self.state == State::Maximized || self.pending_outer.is_some()
    }

    /// 最小化（记住当前状态供还原）；已最小化时无操作。
    pub fn minimize(&mut self) {
        if self.state != State::Minimized {
            self.restore_state = self.state;
            self.state = State::Minimized;
        }
    }

    /// 从最小化还原到最小化前的状态（内容尺寸未变，无需 `CONFIGURE`）；
    /// 非最小化时无操作。
    pub fn unminimize(&mut self) {
        if self.state == State::Minimized {
            self.state = self.restore_state;
        }
    }

    /// Normal ↔ Maximized 切换，返回应向客户端发送的 `CONFIGURE` 建议内容
    /// 尺寸（`None` 表示无需发送）：
    ///
    /// 1. 进入最大化：记录外框原点与内容尺寸，外框套用 work area；建议尺寸
    ///    为 work area 减去 chrome（客户端 `SET_BUFFER` 后内容填满工作区）。
    /// 2. 还原：回到记录的原点，但视觉外框保持 work area（`pending_outer`），
    ///    直到客户端按建议尺寸（最大化前记录的内容尺寸）`SET_BUFFER` 后外框
    ///    才按新内容自适应；客户端不响应时窗口保持当前视觉，不等待。
    pub fn toggle_maximize(&mut self, work_area: Rect) -> Option<(u32, u32)> {
        match self.state {
            State::Normal => {
                self.saved_origin = (self.x, self.y);
                self.saved_content = (self.content_width, self.content_height);
                self.pending_outer = None;
                self.x = work_area.x1;
                self.y = work_area.y1;
                self.max_rect = work_area;
                self.state = State::Maximized;
                let width = (work_area.width() - 2 * chrome::BORDER).max(1) as u32;
                let height =
                    (work_area.height() - chrome::TITLE_HEIGHT - chrome::BORDER).max(1) as u32;
                Some((width, height))
            }
            State::Maximized => {
                (self.x, self.y) = self.saved_origin;
                self.pending_outer = Some(self.max_rect);
                self.state = State::Normal;
                Some((self.saved_content.0 as u32, self.saved_content.1 as u32))
            }
            State::Minimized => None,
        }
    }

    pub fn title(&self) -> &[u8] {
        &self.title[..self.title_len]
    }

    /// 更新标题（超过 [`MAX_TITLE`] 截断）。
    pub fn set_title(&mut self, title: &[u8]) {
        let keep = title.len().min(MAX_TITLE);
        self.title[..keep].copy_from_slice(&title[..keep]);
        self.title_len = keep;
    }

    /// 内容第 `y` 行像素（`y < content_height`，越界属编程错误）。
    pub fn content_row(&self, y: usize) -> &[u32] {
        assert!(y < self.content_height);
        // SAFETY: pixels 指向 `content_width * 4 * content_height` 字节的
        // 共享映射；窗口存活期间映射不释放（销毁先经 Windows::remove），
        // 合成单线程进行，读切片不与任何写别名。
        unsafe {
            core::slice::from_raw_parts(
                (self.pixels as *const u8)
                    .add(y * self.content_width * 4)
                    .cast::<u32>(),
                self.content_width,
            )
        }
    }

    /// 替换 backing buffer（`SET_BUFFER`）：采用新映射与新内容尺寸（锚定左上
    /// 角不变），并 unmap + `DESTROY_DUMB` 旧 handle（旧 handle 所有权在桌面）。
    ///
    /// `pixels` / `map_size` 为已完成的新 buffer 映射。
    pub fn apply_buffer(
        &mut self,
        drm_fd: i32,
        gem_handle: u32,
        pixels: *mut u32,
        map_size: usize,
        width: usize,
        height: usize,
    ) {
        // SAFETY: pixels/map_size 为本窗口持有的旧映射；旧 handle 所有权在桌面。
        unsafe { ffi::munmap(self.pixels.cast(), self.map_size) };
        scanout::destroy_dumb(drm_fd, self.gem_handle);
        self.gem_handle = gem_handle;
        self.pixels = pixels;
        self.map_size = map_size;
        self.content_width = width;
        self.content_height = height;
        // 还原过渡期的视觉外框到此为止：外框按新内容尺寸自适应（最大化状态
        // 下 pending_outer 恒为 None，外框仍保持 work area）。
        self.pending_outer = None;
    }

    /// 释放映射并销毁 GEM handle（handle 所有权归桌面）。
    fn destroy(&mut self, drm_fd: i32) {
        if !self.alive {
            return;
        }
        // SAFETY: pixels/map_size 为本窗口持有的映射；handle 所有权在桌面。
        unsafe { ffi::munmap(self.pixels.cast(), self.map_size) };
        scanout::destroy_dumb(drm_fd, self.gem_handle);
        self.alive = false;
        self.pixels = core::ptr::null_mut();
    }
}

/// [`Windows::add`] 的注册参数（参数对象，避免 9 参数签名）。
pub struct SurfaceDesc<'a> {
    /// 拥有者 client 在 `Clients` 中的索引。
    pub client: usize,
    /// 客户端创建、所有权已转移给桌面的 GEM handle。
    pub gem_handle: u32,
    /// 客户端 buffer 的共享映射指针。
    pub pixels: *mut u32,
    /// 映射字节数（`width * 4 * height`）。
    pub map_size: usize,
    /// 内容宽度（像素）。
    pub width: usize,
    /// 内容高度（像素）。
    pub height: usize,
    /// 是否带 SSD 装饰。
    pub decorated: bool,
    /// 初始标题（超过 [`MAX_TITLE`] 截断）。
    pub title: &'a [u8],
}

/// 窗口集 + z-order 栈 + 焦点。
pub struct Windows {
    list: [Window; MAX_WINDOWS],
    order: [usize; MAX_WINDOWS],
    count: usize,
    focused: Option<usize>,
    next_surface_id: u32,
}

impl Windows {
    pub fn new() -> Self {
        Self {
            list: [Window::EMPTY; MAX_WINDOWS],
            order: [0; MAX_WINDOWS],
            count: 0,
            focused: None,
            next_surface_id: 1,
        }
    }

    /// 注册新窗口并置顶（焦点由调用方设置）。
    ///
    /// `desc.pixels` / `desc.map_size` 为已完成的客户端 buffer 映射。返回槽位
    /// 与分配的 surface id；窗口满时返回 `None`（映射与 handle 仍归调用方清理）。
    pub fn add(&mut self, desc: SurfaceDesc<'_>) -> Option<(usize, u32)> {
        let slot = self.list.iter().position(|window| !window.alive)?;
        let surface_id = self.next_surface_id;
        self.next_surface_id = self.next_surface_id.wrapping_add(1).max(1);
        // 级联初始位置（1× 基准 48 + 24 步进，按 SCALE 缩放），避免多窗完全重叠。
        let cascade = (48 + 24 * (surface_id as i32 % 8)) * chrome::SCALE;
        let mut window = Window {
            surface_id,
            client: desc.client,
            gem_handle: desc.gem_handle,
            pixels: desc.pixels,
            map_size: desc.map_size,
            content_width: desc.width,
            content_height: desc.height,
            x: cascade,
            y: cascade,
            decorated: desc.decorated,
            alive: true,
            ..Window::EMPTY
        };
        window.set_title(desc.title);
        self.list[slot] = window;
        self.order[self.count] = slot;
        self.count += 1;
        Some((slot, surface_id))
    }

    pub fn get(&self, slot: usize) -> Option<&Window> {
        self.list.get(slot).filter(|window| window.alive)
    }

    pub fn get_mut(&mut self, slot: usize) -> Option<&mut Window> {
        self.list.get_mut(slot).filter(|window| window.alive)
    }

    pub fn by_surface(&self, surface_id: u32) -> Option<usize> {
        self.list
            .iter()
            .position(|window| window.alive && window.surface_id == surface_id)
    }

    /// 栈顶可见（未最小化）窗口槽位；焦点回落时使用。
    pub fn top_visible(&self) -> Option<usize> {
        self.order[..self.count]
            .iter()
            .rev()
            .copied()
            .find(|slot| self.list[*slot].state != State::Minimized)
    }

    /// 按槽位顺序（创建顺序，槽位复用除外）收集存活窗口槽位，供任务栏按钮
    /// 这类需要稳定顺序的 UI 使用；返回写入数量。
    pub fn ordered_slots(&self, out: &mut [usize; MAX_WINDOWS]) -> usize {
        let mut count = 0;
        for (slot, window) in self.list.iter().enumerate() {
            if window.alive {
                out[count] = slot;
                count += 1;
            }
        }
        count
    }

    pub fn focused(&self) -> Option<usize> {
        self.focused
    }

    pub fn set_focus(&mut self, slot: Option<usize>) {
        self.focused = slot;
    }

    /// 把窗口提到栈顶。
    pub fn raise(&mut self, slot: usize) {
        let Some(index) = self.order[..self.count].iter().position(|entry| *entry == slot)
        else {
            return;
        };
        // 子切片左旋 1：槽位移到栈尾（栈顶），其余相对顺序不变。
        self.order[index..self.count].rotate_left(1);
    }

    /// 底→顶遍历槽位。
    pub fn bottom_to_top(&self) -> &[usize] {
        &self.order[..self.count]
    }

    /// 销毁窗口并从栈中移除；焦点落在该窗口时清空（调用方另行聚焦栈顶）。
    pub fn remove(&mut self, slot: usize, drm_fd: i32) {
        if self.list[slot].alive {
            self.list[slot].destroy(drm_fd);
        }
        if let Some(index) = self.order[..self.count].iter().position(|entry| *entry == slot) {
            self.order[index..self.count].rotate_left(1);
            self.count -= 1;
        }
        if self.focused == Some(slot) {
            self.focused = None;
        }
    }

    /// 命中测试：返回最上层包含 `(x, y)` 的窗口及其区域；最小化窗口不参与，
    /// 左 / 上边框归入 TitleBar，右 / 下边缘与右下角为缩放命中带（最大化窗口
    /// 无缩放命中带），标题栏按钮优先于其他区域。
    pub fn hit_test(&self, x: i32, y: i32) -> Option<(usize, Region)> {
        for slot in self.bottom_to_top().iter().rev().copied() {
            let Some(window) = self.get(slot) else {
                continue;
            };
            if window.state == State::Minimized {
                continue;
            }
            let outer = window.outer_rect();
            if !outer.contains(x, y) {
                continue;
            }
            if !window.decorated {
                return Some((slot, Region::Content));
            }
            let layout = window.layout();
            let origin = (window.x, window.y);
            for (rect, region) in [
                (layout.close_button, Region::CloseButton),
                (layout.maximize_button, Region::MaximizeButton),
                (layout.minimize_button, Region::MinimizeButton),
            ] {
                if shift(rect, origin).contains(x, y) {
                    return Some((slot, region));
                }
            }
            if !window.geometry_locked() {
                let band_x = outer.x2 - RESIZE_BAND;
                let band_y = outer.y2 - RESIZE_BAND;
                if x >= band_x && y >= band_y {
                    return Some((slot, Region::ResizeSouthEast));
                }
                if x >= band_x && y >= outer.y1 + chrome::TITLE_HEIGHT {
                    return Some((slot, Region::ResizeEast));
                }
                if y >= band_y {
                    return Some((slot, Region::ResizeSouth));
                }
            }
            let title_bar = shift(layout.title_bar, origin);
            let content = window.content_rect();
            if title_bar.contains(x, y) || !content.contains(x, y) {
                return Some((slot, Region::TitleBar));
            }
            return Some((slot, Region::Content));
        }
        None
    }
}

fn shift(rect: Rect, origin: (i32, i32)) -> Rect {
    Rect::new(
        rect.x1 + origin.0,
        rect.y1 + origin.1,
        rect.x2 + origin.0,
        rect.y2 + origin.1,
    )
}
