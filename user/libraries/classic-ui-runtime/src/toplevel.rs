//! 稳定边界只投影标准 foreign-toplevel 的快照和请求；协议事实仍由 Display 独占。
use super::{Context, copy_text, invalid, invoke};
use classic_ui::{Toplevel, ToplevelId, ToplevelRequest};
use classic_ui_abi as abi;
use std::{ffi::c_void, io};
impl Context {
    fn reconcile_foreign(&mut self) -> io::Result<()> {
        let live: Vec<_> = self.display.toplevels().map(|(id, _)| id).collect();
        self.foreign.retain(|_, id| live.contains(id));
        for id in live {
            if !self.foreign.values().any(|value| *value == id) {
                let token = self.next_foreign;
                self.next_foreign = token
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("foreign identities exhausted"))?;
                self.foreign.insert(token, id);
            }
        }
        Ok(())
    }
    fn foreign_target(&self, token: u64) -> io::Result<ToplevelId> {
        self.foreign
            .get(&token)
            .copied()
            .filter(|id| self.display.toplevels().any(|(live, _)| live == *id))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "toplevel has closed"))
    }
    fn foreign_info(&self, token: u64) -> io::Result<&Toplevel> {
        let target = self.foreign_target(token)?;
        self.display
            .toplevels()
            .find_map(|(id, info)| (id == target).then_some(info))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "toplevel has closed"))
    }
}
pub(super) unsafe extern "C" fn toplevel_ids(
    pointer: *mut c_void,
    buffer: *mut u64,
    capacity: usize,
    written: *mut usize,
) -> i32 {
    if written.is_null() || (capacity != 0 && buffer.is_null()) {
        return abi::INVALID;
    }
    // SAFETY: context 独占，两个输出由调用方独占；只复制已经完成 done 的当前标识。
    unsafe {
        let status = invoke(pointer, |context| context.reconcile_foreign());
        if status != abi::OK {
            return status;
        }
        let context = &*pointer.cast::<Context>();
        written.write(context.foreign.len());
        if capacity < context.foreign.len() {
            return abi::TOO_SMALL;
        }
        for (index, id) in context.foreign.keys().enumerate() {
            buffer.add(index).write(*id);
        }
    }
    abi::OK
}
pub(super) unsafe extern "C" fn toplevel_text(
    pointer: *mut c_void,
    id: u64,
    field: u32,
    buffer: *mut u8,
    capacity: usize,
    written: *mut usize,
) -> i32 {
    // SAFETY: 借用仅在本次 ABI 调用内有效，不持有任何应用对象或回调。
    unsafe {
        let Some(context) = pointer.cast::<Context>().as_ref() else {
            return abi::INVALID;
        };
        let Ok(info) = context.foreign_info(id) else {
            return abi::NOT_FOUND;
        };
        let text = match field {
            0 => &info.title,
            1 => &info.app_id,
            _ => return abi::INVALID,
        };
        copy_text(text, buffer, capacity, written)
    }
}
pub(super) unsafe extern "C" fn toplevel_state(
    pointer: *mut c_void,
    id: u64,
    output: *mut u32,
) -> i32 {
    if output.is_null() {
        return abi::INVALID;
    }
    // SAFETY: 输出独占，snapshot 在调用期间不经第二个 reader 更新。
    unsafe {
        invoke(pointer, |context| {
            let info = context.foreign_info(id)?;
            output.write(
                u32::from(info.maximized)
                    | (u32::from(info.minimized) << 1)
                    | (u32::from(info.active) << 2)
                    | (u32::from(info.fullscreen) << 3),
            );
            Ok(())
        })
    }
}
pub(super) unsafe extern "C" fn toplevel_action(pointer: *mut c_void, id: u64, action: u32) -> i32 {
    // SAFETY: 请求参数先验证，已 closed 或复用过的协议编号不能命中旧 SDK token。
    unsafe {
        invoke(pointer, |context| {
            let target = context.foreign_target(id)?;
            let request = match action {
                0 => ToplevelRequest::Activate,
                1 => ToplevelRequest::Close,
                2 => ToplevelRequest::Minimize(true),
                3 => ToplevelRequest::Minimize(false),
                4 => ToplevelRequest::Maximize(true),
                5 => ToplevelRequest::Maximize(false),
                _ => return Err(invalid("unknown toplevel action")),
            };
            context.display.control_toplevel(target, request)
        })
    }
}
