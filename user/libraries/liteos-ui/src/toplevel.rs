//! 窗口列表和控制只消费共享运行库已经完成的标准 foreign-toplevel 快照。
use crate::{Application, Error, Result, connection::Connection};
use classic_ui_abi as abi;
use std::rc::Rc;
/// 完整 done 后的顶层窗口快照；请求结果仍以后续 ToplevelsChanged 为准。
#[derive(Clone)]
pub struct Toplevel {
    connection: Rc<Connection>,
    id: u64,
    pub title: String,
    pub app_id: String,
    pub maximized: bool,
    pub minimized: bool,
    pub active: bool,
    pub fullscreen: bool,
}
/// 标准窗口控制请求，不承诺 compositor 必须接受策略请求。
#[derive(Clone, Copy)]
pub enum ToplevelRequest {
    Activate,
    Close,
    Minimize(bool),
    Maximize(bool),
}
impl Toplevel {
    /// 本 Application 中不复用的标识；closed 后旧快照请求返回 NotFound。
    pub fn id(&self) -> u64 {
        self.id
    }
    /// 发送标准控制请求；closed、连接或协议失败返回错误，不把发送成功当成实际状态变化。
    pub fn request(&self, request: ToplevelRequest) -> Result<()> {
        let action = match request {
            ToplevelRequest::Activate => 0,
            ToplevelRequest::Close => 1,
            ToplevelRequest::Minimize(true) => 2,
            ToplevelRequest::Minimize(false) => 3,
            ToplevelRequest::Maximize(true) => 4,
            ToplevelRequest::Maximize(false) => 5,
        };
        let c = &self.connection;
        // SAFETY: 句柄来自此 context 的完整快照；运行库验证仍存活的协议代际。
        c.check(unsafe { (c.api.toplevel_action)(c.context.as_ptr(), self.id, action) })
    }
}
impl Application {
    /// 读取标准窗口列表的完整快照；仅分配当前条目数量，容量查询不推进第二个协议 reader。
    pub fn toplevels(&self) -> Result<Vec<Toplevel>> {
        let c = &self.connection;
        let mut count = 0;
        // SAFETY: 零容量查询不解引用空 buffer；输出数量独占。
        let code = unsafe {
            (c.api.toplevel_ids)(c.context.as_ptr(), std::ptr::null_mut(), 0, &mut count)
        };
        if code != abi::TOO_SMALL {
            c.check(code)?;
        }
        if count > 65536 {
            return Err(Error {
                code: abi::FAILED,
                message: "window catalogue exceeds SDK capacity".into(),
            });
        }
        let mut ids = vec![0; count];
        // SAFETY: 数组容量与 written 独占；同线程调用之间不分发协议事件，快照数量保持稳定。
        c.check(unsafe {
            (c.api.toplevel_ids)(c.context.as_ptr(), ids.as_mut_ptr(), ids.len(), &mut count)
        })?;
        ids.truncate(count);
        ids.into_iter()
            .map(|id| {
                let text = |field| {
                    c.read(|buffer, len, written| {
                        // SAFETY: read 提供独占缓冲，本次调用复制运行库字符串，不保留借用。
                        unsafe {
                            (c.api.toplevel_text)(
                                c.context.as_ptr(),
                                id,
                                field,
                                buffer,
                                len,
                                written,
                            )
                        }
                    })
                };
                let mut state = 0;
                // SAFETY: 状态为独占标量输出，标识来自上面的同一 context 快照。
                c.check(unsafe { (c.api.toplevel_state)(c.context.as_ptr(), id, &mut state) })?;
                Ok(Toplevel {
                    connection: c.clone(),
                    id,
                    title: text(0)?,
                    app_id: text(1)?,
                    maximized: state & 1 != 0,
                    minimized: state & 2 != 0,
                    active: state & 4 != 0,
                    fullscreen: state & 8 != 0,
                })
            })
            .collect()
    }
}
