//! 经可信会话委派给 Shell 的版本化批量窗口接口；状态与窗口栈仍由 Session 拥有。
use super::{
    Client, ForeignRequest, WindowRequest,
    client::{Data, Fail, Privilege},
};
use wayland_wire::{protocol::liteos_shell_v1, wire::ObjectId};

/// 同一连接的窗口请求按线序进入唯一队列，不能把外壳命令排到逐窗口操作之前或之后。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowCommand {
    Toplevel(ObjectId, WindowRequest),
    Foreign(ForeignRequest),
    Shell(liteos_shell_v1::Action),
}

impl Client {
    pub(super) fn shell_control(
        &mut self,
        _object: ObjectId,
        request: liteos_shell_v1::Request,
    ) -> Result<(), Fail> {
        if self.privilege != Privilege::Shell {
            return Err(Fail::invalid_object("shell authorization required"));
        }
        match request {
            liteos_shell_v1::Request::Destroy => {}
            liteos_shell_v1::Request::Perform { action } => {
                self.requests.push(WindowCommand::Shell(action))
            }
        }
        Ok(())
    }

    /// 是否仍有已授权的外壳控制对象；最后一个对象销毁与断连都撤销其批量状态。
    pub fn has_shell_control(&self) -> bool {
        self.privilege == Privilege::Shell
            && !self.dead
            && self
                .objects
                .values()
                .any(|entry| matches!(entry.data, Data::Shell(_)))
    }

    /// 只保存最近发送的 wire 状态，不能用该缓存决定批量操作或撤销成员。
    pub fn sync_shell(&mut self, flags: u32) {
        for (id, entry) in &mut self.objects {
            if let Data::Shell(sent) = &mut entry.data
                && *sent != Some(flags)
            {
                let _ = liteos_shell_v1::Event::State { flags }.encode(
                    ObjectId::new(*id).expect("live object id"),
                    &mut self.writer,
                );
                *sent = Some(flags);
            }
        }
    }
}
