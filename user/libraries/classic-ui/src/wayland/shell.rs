//! 仅给可信会话委派的 LiteOS 外壳使用的批量窗口事务，不暴露普通应用能力。
use super::Display;
/// 版本化系统外壳命令；只经已授权的标准 Wayland socket 发送。
pub use liteos_shell_v1::{Action as ShellCommand, Arrangement as ShellArrangement};
use std::io;
use wayland_wire::{
    protocol::{AnyEvent, liteos_shell_v1},
    wire::ObjectId,
};

impl Display {
    /// 请求合成器原子执行批量窗口操作；Ok 仅表示已发送，实际状态以后续 state 为准。
    /// `action` 的切换由服务端决定，不依赖异步快照。普通连接返回 PermissionDenied，编码/传输错误原样返回。
    pub fn control_shell(&mut self, action: ShellCommand) -> io::Result<()> {
        let (object, _) = self.shell.ok_or(io::ErrorKind::PermissionDenied)?;
        liteos_shell_v1::Request::Perform { action }
            .encode(object, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        self.transport.flush()
    }

    /// 最后一次完整服务端快照是否允许撤销最小化；未授权或尚未收到快照时为 false，无错误返回。
    pub fn can_undo_minimize_all(&self) -> bool {
        self.shell
            .is_some_and(|(_, flags)| flags & liteos_shell_v1::state::CANUNDO != 0)
    }

    /// 排列当前可见窗口；`layout` 决定层叠或平铺方向。成员与约束由服务端裁决。
    /// Ok 表示已发送；未授权返回 PermissionDenied，编码/传输错误原样返回。
    pub fn arrange_windows(&mut self, layout: ShellArrangement) -> io::Result<()> {
        let (object, _) = self.shell.ok_or(io::ErrorKind::PermissionDenied)?;
        liteos_shell_v1::Request::Arrange { layout }
            .encode(object, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        self.transport.flush()
    }

    /// 撤销该连接最近的排列；无可撤销批次时服务端不作改动。
    /// 未授权返回 PermissionDenied，编码/传输错误原样返回。
    pub fn undo_arrangement(&mut self) -> io::Result<()> {
        let (object, _) = self.shell.ok_or(io::ErrorKind::PermissionDenied)?;
        liteos_shell_v1::Request::UndoArrange
            .encode(object, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        self.transport.flush()
    }

    /// 最近快照的可撤销排列；未授权、无批次或成员全部失效时为 None，无错误返回。
    pub fn undo_arrangement_kind(&self) -> Option<ShellArrangement> {
        use liteos_shell_v1::state;
        let (_, flags) = self.shell?;
        if flags & state::UNDOCASCADE != 0 {
            Some(ShellArrangement::Cascade)
        } else if flags & state::UNDOHORIZONTAL != 0 {
            Some(ShellArrangement::Horizontal)
        } else if flags & state::UNDOVERTICAL != 0 {
            Some(ShellArrangement::Vertical)
        } else {
            None
        }
    }

    pub(super) fn shell_event(&mut self, object: ObjectId, event: &AnyEvent) -> bool {
        let AnyEvent::LiteosShellV1(liteos_shell_v1::Event::State { flags }) = event else {
            return false;
        };
        if let Some((bound, current)) = &mut self.shell
            && *bound == object
            && current != flags
        {
            *current = *flags;
            self.foreign_revision = self.foreign_revision.wrapping_add(1);
            self.emit(
                self.windows.keys().copied().next(),
                super::EventKind::ToplevelsChanged,
            );
        }
        true
    }
}
