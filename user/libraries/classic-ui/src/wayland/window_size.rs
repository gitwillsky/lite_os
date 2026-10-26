//! 固定尺寸顶层窗口的标准 xdg-shell 约束与内容尺寸；配置仍由 Display 唯一持有。
use super::{Display, Event, EventKind, Role, WindowId};
use std::io;
use wayland_wire::protocol::xdg_toplevel;

impl Display {
    /// 声明固定尺寸：最小与最大均为当前首选尺寸；应在首次映射前调用。
    /// 未知窗口、非 xdg_toplevel 或协议 I/O 失败返回错误。
    pub fn fix_size(&mut self, window: WindowId) -> io::Result<()> {
        let state = self
            .windows
            .get(&window)
            .ok_or_else(|| io::Error::other("unknown window"))?;
        self.fixed_size_limits(
            window,
            (state.configuration.width, state.configuration.height),
        )
    }

    /// 为固定尺寸窗口容纳内容而扩展 surface 的首选逻辑尺寸，同时更新标准 min/max 约束。
    /// 普通状态下 xdg_toplevel.configure 的尺寸是建议；最大化/全屏/交互缩放期间由合成器决定尺寸。
    /// 返回是否扩大；非法尺寸、未知窗口、非 toplevel 或协议失败返回错误。
    pub fn grow_fixed_size(&mut self, window: WindowId, size: (i32, i32)) -> io::Result<bool> {
        let state = self
            .windows
            .get(&window)
            .ok_or_else(|| io::Error::other("unknown window"))?;
        let mut next = state.configuration;
        if !state.configured || next.maximized || next.fullscreen || next.resizing {
            return Ok(false);
        }
        next.width = next.width.max(size.0);
        next.height = next.height.max(size.1);
        next.pixel_size()?;
        if next == state.configuration {
            return Ok(false);
        }
        self.fixed_size_limits(window, (next.width, next.height))?;
        let state = self.windows.get_mut(&window).expect("validated window");
        state.configuration = next;
        if let Role::Toplevel { pending, .. } = &mut state.role {
            pending.width = pending.width.max(next.width);
            pending.height = pending.height.max(next.height);
        }
        self.events.push_back(Event {
            window,
            kind: EventKind::Configure(next),
        });
        Ok(true)
    }

    fn fixed_size_limits(&mut self, window: WindowId, size: (i32, i32)) -> io::Result<()> {
        let toplevel = match self.windows.get(&window).map(|state| &state.role) {
            Some(Role::Toplevel { toplevel, .. }) => *toplevel,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "fixed size needs an xdg toplevel",
                ));
            }
        };
        let (width, height) = size;
        xdg_toplevel::Request::SetMinSize { width, height }
            .encode(toplevel, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        xdg_toplevel::Request::SetMaxSize { width, height }
            .encode(toplevel, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        // 两个约束随下一帧 wl_surface.commit 一起生效，期间不提交不一致的尺寸对。
        self.transport.flush()
    }
}
