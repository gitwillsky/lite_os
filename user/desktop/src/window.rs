//! 窗口对象与 z-order 栈。
//!
//! 窗口上限 8，固定数组 + 空闲槽管理（无堆分配）。`order` 为 z-order 栈
//! （底→顶，顶在尾），`focused` 记录键盘焦点窗口的槽位。
//!
//! 窗口内容像素来自客户端的 dumb buffer：`CREATE_SURFACE` 提及时 handle
//! 所有权转移给桌面，桌面 `MAP_DUMB` + `mmap` 后只读合成；窗口销毁时由桌面
//! `munmap` + `DESTROY_DUMB`（客户端绝不销毁 handle）。内核 dumb pitch 恒为
//! `width * 4`，故映射大小为 `width * 4 * height`。

use crate::{
    chrome::{self, Layout},
    ffi,
    scanout::{self, Rect},
};

/// 同时存在的窗口上限（第一期单 / 少窗口场景）。
pub const MAX_WINDOWS: usize = 8;
/// 标题字节上限（超出截断）。
pub const MAX_TITLE: usize = 64;

/// hit-test 结果：指针落在窗口的哪个区域。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// 关闭按钮。
    CloseButton,
    /// 标题栏（含边框，可拖动）。
    TitleBar,
    /// 内容区。
    Content,
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
        alive: false,
    };

    /// 装饰布局（相对外框原点）。
    pub fn layout(&self) -> Layout {
        chrome::layout(
            self.content_width as i32,
            self.content_height as i32,
            self.decorated,
        )
    }

    /// 外框的屏幕矩形。
    pub fn outer_rect(&self) -> Rect {
        let layout = self.layout();
        Rect::new(
            self.x,
            self.y,
            self.x + layout.outer_width,
            self.y + layout.outer_height,
        )
    }

    /// 内容区的屏幕矩形。
    pub fn content_rect(&self) -> Rect {
        let layout = self.layout();
        let (ox, oy) = layout.content_origin;
        Rect::new(
            self.x + ox,
            self.y + oy,
            self.x + ox + self.content_width as i32,
            self.y + oy + self.content_height as i32,
        )
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
        // 级联初始位置，避免多窗完全重叠。
        let cascade = 48 + 24 * (surface_id as i32 % 8);
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

    /// 栈顶窗口槽位。
    pub fn top(&self) -> Option<usize> {
        self.count.checked_sub(1).map(|index| self.order[index])
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

    /// 命中测试：返回最上层包含 `(x, y)` 的窗口及其区域；边框归入 TitleBar。
    pub fn hit_test(&self, x: i32, y: i32) -> Option<(usize, Region)> {
        for slot in self.bottom_to_top().iter().rev().copied() {
            let Some(window) = self.get(slot) else {
                continue;
            };
            let outer = window.outer_rect();
            if !outer.contains(x, y) {
                continue;
            }
            if !window.decorated {
                return Some((slot, Region::Content));
            }
            let layout = window.layout();
            let close = shift(layout.close_button, (window.x, window.y));
            let title_bar = shift(layout.title_bar, (window.x, window.y));
            let content = window.content_rect();
            if close.contains(x, y) {
                return Some((slot, Region::CloseButton));
            }
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
